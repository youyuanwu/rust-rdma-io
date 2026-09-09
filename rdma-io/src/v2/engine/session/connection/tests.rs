#![cfg(test)]

use super::super::super::io::{IoEvent, event_port};
use super::super::registry::ConnectionRegistry;
use super::*;
use crate::wr::{RecvWr, SendWr, WrOpcode};

struct TestPoster;

impl WorkRequestPoster for TestPoster {
    fn qp_num(&self) -> u32 {
        1
    }

    fn capabilities(&self) -> Option<QpCapabilities> {
        None
    }

    fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        Ok(BatchPostOutcome::AllAccepted)
    }

    fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        Ok(BatchPostOutcome::AllAccepted)
    }

    fn to_error(
        &self,
        _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
    ) -> Result<()> {
        Ok(())
    }

    fn destroy_qp(
        &self,
        _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
    ) -> Result<bool> {
        Ok(false)
    }

    fn disconnect(&self) -> Result<()> {
        Ok(())
    }
}

#[test]
fn posting_authority_is_borrowed_from_the_entry_owned_poster() {
    let owner: Arc<dyn WorkRequestPoster> = Arc::new(TestPoster);
    let weak = Arc::downgrade(&owner);
    let authority = ConnectionPoster::from(owner);
    assert_eq!(authority.qp_num(), 1);
    assert_eq!(
        weak.strong_count(),
        1,
        "the connection entry is the sole owning poster"
    );

    drop(authority);
    assert!(weak.upgrade().is_none());
}

#[test]
fn live_io_proofs_require_exact_identity() {
    let mut registry = ConnectionRegistry::new(1).unwrap();
    let registration = registry.register(1, |token| {
        ConnectionState::new(
            token,
            Arc::new(TestPoster),
            RdmaConnectionConfig::default(),
            None,
            None,
            None,
        )
    });
    let (token, _connection) = match registration {
        Ok(registered) => registered,
        Err(failure) => panic!("connection registration failed: {}", failure.error),
    };

    let live = registry.prove_live_io(token, 1).expect("exact live proof");
    assert!(live.proves(token, 1));
    assert!(registry.prove_live_io(token, 2).is_none());
    assert_eq!(
        registry
            .with_connection(token, |connection| connection.io.identity())
            .unwrap(),
        EstablishedIoIdentity {
            connection: token,
            qp_num: 1,
        }
    );

    let wrong_generation = ConnectionToken {
        slot: token.slot,
        generation: token.generation + 1,
    };
    registry.set_qp_mapping_for_test(1, wrong_generation);
    assert!(registry.prove_live_io(token, 1).is_none());

    registry.set_qp_mapping_for_test(1, token);
    assert!(registry.begin_close(token));
    registry
        .with_connection_mut(token, |connection| {
            connection
                .transition_to_error_once(&SessionLifecycleAuthority::for_test())
                .unwrap();
        })
        .unwrap();
    assert!(registry.request_retirement(token));
    assert!(registry.begin_retirement(token));
    let retained = registry
        .release(token, 1)
        .expect("the exact live generation remains registered");
    registry.set_qp_mapping_for_test(1, token);
    assert!(
        registry.prove_live_io(token, 1).is_none(),
        "a released generation cannot mint a live I/O proof even when its QP mapping remains"
    );
    assert!(registry.detach_qp_index(token, 1));

    let replacement = registry.register(1, |replacement| {
        ConnectionState::new(
            replacement,
            Arc::new(TestPoster),
            RdmaConnectionConfig::default(),
            None,
            None,
            None,
        )
    });
    let (replacement, _) = match replacement {
        Ok(registered) => registered,
        Err(failure) => panic!("replacement registration failed: {}", failure.error),
    };
    assert_eq!(replacement.slot, token.slot);
    assert_ne!(replacement.generation, token.generation);
    assert!(registry.prove_live_io(token, 1).is_none());
    assert!(
        registry
            .prove_live_io(replacement, 1)
            .is_some_and(|proof| proof.proves(replacement, 1))
    );
    drop(retained);
}

#[test]
fn terminal_events_are_pending_until_backend_state_is_committed() {
    let mut connection = ConnectionState::new(
        ConnectionToken {
            slot: 0,
            generation: 1,
        },
        Arc::new(TestPoster),
        RdmaConnectionConfig::default(),
        None,
        None,
        None,
    );
    let (sender, receiver) = event_port();
    assert!(
        connection
            .install_io_event_sender(sender)
            .unwrap()
            .is_none()
    );

    for pending in [
        connection.mark_disconnected(),
        connection.mark_cm_failure(Error::DriverShutdown),
        connection.finish_retirement(),
    ] {
        let pending = pending.expect("attached event port");
        assert!(!receiver.has_events());
        assert!(
            connection.io.io_event_lock_available(),
            "terminal event was prepared while the sender lock remained held"
        );
        pending.deliver();
        assert!(matches!(receiver.pop(), Some(IoEvent::Terminal(_))));
    }
}

#[test]
fn returned_qp_capabilities_must_not_be_reduced() {
    let config = RdmaConnectionConfig::default();
    QpCapabilities {
        max_send_wr: 19,
        max_recv_wr: 34,
        max_send_sge: 1,
        max_recv_sge: 1,
    }
    .require(&config)
    .unwrap();
    assert!(
        QpCapabilities {
            max_send_wr: 18,
            max_recv_wr: 34,
            max_send_sge: 1,
            max_recv_sge: 1,
        }
        .require(&config)
        .is_err()
    );
}

#[test]
fn public_connection_surface_is_send_sync_without_raw_accessors() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<RdmaConnection>();
    let _: fn(&RdmaConnection) -> RdmaConnectionIdentity = RdmaConnection::identity;
    let _: fn(&RdmaConnection) -> Result<SocketAddr> = RdmaConnection::local_addr;
    let _: fn(&RdmaConnection) -> Result<SocketAddr> = RdmaConnection::peer_addr;
}

#[test]
fn memoized_close_failure_preserves_its_typed_error() {
    let outcome =
        MemoizedTerminalResult::from_error(Error::InvalidConfig("typed close failure".into()));
    for result in [outcome.clone().into_result(), outcome.into_result()] {
        assert!(
            matches!(result, Err(Error::InvalidConfig(ref message)) if message == "typed close failure")
        );
    }
}

#[test]
fn destroy_quarantine_publishes_event_and_outcome_once() {
    let mut connection = ConnectionState::new(
        ConnectionToken {
            slot: 0,
            generation: 1,
        },
        Arc::new(TestPoster),
        RdmaConnectionConfig::default(),
        None,
        None,
        None,
    );
    let publications = AtomicUsize::new(0);

    assert!(
        connection
            .publish_destroy_quarantine(
                &Error::InvalidConfig("first destroy failure".into()),
                || {
                    publications.fetch_add(1, Ordering::Relaxed);
                },
            )
            .0
    );
    assert!(
        !connection
            .publish_destroy_quarantine(
                &Error::InvalidConfig("repeated destroy failure".into()),
                || {
                    publications.fetch_add(1, Ordering::Relaxed);
                },
            )
            .0
    );

    assert_eq!(publications.load(Ordering::Relaxed), 1);
    assert!(matches!(
        connection.close_outcome().unwrap().into_result(),
        Err(Error::ConnectionDestroyQuarantined { cause })
            if cause.contains("first destroy failure")
    ));
}

#[test]
fn destroy_with_accepted_work_fails_closed_without_destroying() {
    struct DestroyPoster(AtomicUsize);

    impl WorkRequestPoster for DestroyPoster {
        fn qp_num(&self) -> u32 {
            7
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn post_recv(&self, _: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn to_error(
            &self,
            _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
        ) -> Result<()> {
            Ok(())
        }

        fn destroy_qp(
            &self,
            _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
        ) -> Result<bool> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(true)
        }

        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    let poster = Arc::new(DestroyPoster(AtomicUsize::new(0)));
    let mut connection = ConnectionState::new(
        ConnectionToken {
            slot: 1,
            generation: 1,
        },
        Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
        RdmaConnectionConfig::default(),
        None,
        None,
        None,
    );
    let authority = SessionLifecycleAuthority::for_test();
    let error = match connection.destroy_connection_resources(&authority, 1) {
        Ok(_) => panic!("accepted work must prevent connection destruction"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 1,
            cq_debt: 1
        }
    ));
    assert_eq!(poster.0.load(Ordering::Acquire), 0);
}

#[test]
fn missing_qp_returns_typed_post_errors() {
    let resources = VerbsConnectionResources {
        qp: None,
        qp_num: 9,
        capabilities: QpCapabilities {
            max_send_wr: 1,
            max_recv_wr: 1,
            max_send_sge: 1,
            max_recv_sge: 1,
        },
        cm_owner: None,
    };
    let mut send =
        PreparedSendBatch::new(vec![SendWr::new(1, WrOpcode::Send)]).expect("send batch");
    let mut recv = PreparedRecvBatch::new(vec![RecvWr::new(2)]).expect("recv batch");

    assert!(matches!(
        resources.post_send_owned(&mut send),
        Err(Error::TransportClosed)
    ));
    assert!(matches!(
        resources.post_recv_owned(&mut recv),
        Err(Error::TransportClosed)
    ));
}
