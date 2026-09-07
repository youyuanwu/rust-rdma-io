#![cfg(test)]

use super::*;
use crate::test_support::destruction::{DestructionKind, DestructionRecorder};
use crate::v2::AccessIntent;
use crate::v2::engine::config::EngineConfig;
use crate::v2::engine::lifecycle::MemoizedTerminalResult;
use crate::v2::engine::session::connection::{WorkRequestPoster, install_connection};
use crate::v2::engine::session::{SessionManager, connection::ConnectionState};
use crate::v2::engine::{EngineShared, SessionEngineRuntime};
use crate::wc::WcStatus;
use rdma_io_sys::ibverbs::{IBV_WC_FATAL_ERR, IBV_WC_RECV, IBV_WC_SEND, IBV_WC_SUCCESS};
use std::sync::Weak;
use std::sync::{Barrier, RwLock};

#[test]
fn all_operation_kinds_preserve_direction_and_opcode_mapping() {
    let mappings = [
        (
            OperationKind::Send,
            Direction::Send,
            WcOpcode::Send,
            Some(WrOpcode::Send),
        ),
        (OperationKind::Recv, Direction::Recv, WcOpcode::Recv, None),
        (
            OperationKind::Write,
            Direction::Send,
            WcOpcode::RdmaWrite,
            Some(WrOpcode::RdmaWrite),
        ),
        (
            OperationKind::Read,
            Direction::Send,
            WcOpcode::RdmaRead,
            Some(WrOpcode::RdmaRead),
        ),
    ];
    for (kind, direction, completion, send_wr) in mappings {
        assert_eq!(kind.direction(), direction);
        assert_eq!(kind.expected_completion_opcode(), completion);
        assert_eq!(kind.send_wr_opcode(), send_wr);
    }
}

#[test]
fn credit_pool_never_oversubscribes_or_reuses_retained_debt() {
    let credits = CqCreditPool::new(2);
    assert!(credits.reserve());
    assert!(credits.reserve());
    assert!(!credits.reserve());
    credits.retain();
    assert_eq!(credits.free(), 0);
    assert_eq!(credits.retained(), 1);
    credits.release();
    assert_eq!(credits.free(), 1);
    assert_eq!(credits.retained(), 1);
    credits.release_retained();
    credits.release();
    assert_eq!(credits.free(), 2);
}

#[test]
fn operation_registry_retires_and_distinguishes_duplicates() {
    let registry = OperationRegistry::new(1).unwrap();
    let connection = synthetic_connection();
    let (token, _) = registry
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                connection,
                Direction::Send,
                WcOpcode::Send,
                None,
                64,
            ))
        })
        .unwrap();
    registry.release(token, true).unwrap();
    assert!(matches!(registry.lookup(token), Lookup::Duplicate));
    let (reused, _) = registry
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                synthetic_connection(),
                Direction::Send,
                WcOpcode::Send,
                None,
                64,
            ))
        })
        .unwrap();
    assert_eq!(reused.slot, token.slot);
    assert_ne!(reused.generation, token.generation);
    assert!(matches!(registry.lookup(token), Lookup::Duplicate));
    registry.release(reused, false).unwrap();
    assert!(matches!(registry.lookup(reused), Lookup::Stale));
}

#[test]
fn operation_lifecycle_transitions_follow_real_post_cancel_and_completion_paths() {
    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 6);
    connection.state.reserve_local(Direction::Send).unwrap();
    assert!(shared.io_core.cq_credits.reserve());
    let (token, operation) = shared
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(&connection.state),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            ))
        })
        .unwrap();
    assert_eq!(operation.lifecycle(), OperationLifecycle::Posting);

    let completion = wc(token, 6, IBV_WC_SEND);
    assert!(operation.mark_completion_queued());
    assert!(matches!(
        operation.record_completion(completion),
        CompletionDisposition::Deferred
    ));
    assert_eq!(operation.lifecycle(), OperationLifecycle::Posting);
    let early = operation.commit_accepted().expect("early completion");
    shared
        .io_core
        .accepted_operations
        .fetch_add(1, Ordering::AcqRel);
    assert_eq!(operation.lifecycle(), OperationLifecycle::Completing);
    let effects = shared
        .io_core
        .finish_operation(Arc::clone(&operation), early);
    shared.session.commit_io_effects(effects);
    assert_eq!(operation.lifecycle(), OperationLifecycle::Released);

    let token = install_accepted(&shared, &connection.state, WcOpcode::Send);
    let Lookup::Occupied(operation) = shared.io_core.operations.lookup(token) else {
        panic!("accepted operation")
    };
    assert_eq!(operation.lifecycle(), OperationLifecycle::InFlight);
    assert!(operation.cancel(&shared.io_core));
    assert_eq!(operation.lifecycle(), OperationLifecycle::Cancelled);
    shared.io_core.begin_reclamation(token);
    assert_eq!(operation.lifecycle(), OperationLifecycle::Reclaiming);
    shared.session.handle_reclamation_deadline(token);
    assert_eq!(operation.lifecycle(), OperationLifecycle::Quarantined);
    let completion = wc(token, 6, IBV_WC_SEND);
    assert!(operation.mark_completion_queued());
    assert!(matches!(
        operation.record_completion(completion),
        CompletionDisposition::Complete
    ));
    assert_eq!(operation.lifecycle(), OperationLifecycle::Completing);
    let effects = shared
        .io_core
        .finish_operation(Arc::clone(&operation), completion);
    shared.session.commit_io_effects(effects);
    assert_eq!(operation.lifecycle(), OperationLifecycle::Released);
}

#[test]
fn io_completion_wakes_after_admission_guard_is_released() {
    use std::task::{Wake, Waker};

    struct AdmissionCheckingWake {
        admission: Arc<RwLock<()>>,
        io_core: Arc<IoCore>,
        connection: Arc<ConnectionState>,
        operation: Arc<OperationState>,
        token: OperationToken,
        observed: AtomicBool,
    }

    impl Wake for AdmissionCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(
                self.admission.try_write().is_ok(),
                "I/O event wake ran while admission remained locked"
            );
            assert!(!matches!(
                self.io_core.operations.lookup(self.token),
                Lookup::Occupied(_)
            ));
            assert_eq!(self.io_core.operations.live(), 0);
            assert_eq!(self.operation.lifecycle(), OperationLifecycle::Released);
            assert_eq!(self.connection.accepted_count(), 0);
            assert_eq!(
                self.connection
                    .io
                    .local_credit_used_for_test(Direction::Send),
                0
            );
            assert_eq!(self.io_core.cq_credits.free(), 8);
            self.observed.store(true, Ordering::Release);
        }
    }

    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 21);
    connection.state.reserve_local(Direction::Send).unwrap();
    assert!(shared.io_core.cq_credits.reserve());
    let (sender, receiver) = super::super::io::event_port();
    let (token, operation) = shared
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new_with_event(
                token,
                Arc::clone(&connection.state),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
                Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
            ))
        })
        .unwrap();
    let wake = Arc::new(AdmissionCheckingWake {
        admission: Arc::clone(&shared.session.admission),
        io_core: Arc::clone(&shared.io_core),
        connection: Arc::clone(&connection.state),
        operation: Arc::clone(&operation),
        token,
        observed: AtomicBool::new(false),
    });
    receiver.register(&Waker::from(Arc::clone(&wake)));
    connection.state.add_accepted(token);
    operation.commit_accepted();
    shared
        .io_core
        .accepted_operations
        .fetch_add(1, Ordering::AcqRel);
    assert_eq!(
        shared
            .session
            .enqueue_completion(wc(token, connection.identity().qp_num(), IBV_WC_SEND,)),
        Some(connection.state.token)
    );

    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 1),
        (1, false)
    );
    assert!(wake.observed.load(Ordering::Acquire));
    assert!(matches!(
        receiver.pop(),
        Some(super::super::io::IoEvent::Completion(_))
    ));
}

#[test]
fn qp_destroy_event_uses_the_contextual_connection_close_error() {
    use std::task::{Wake, Waker};

    struct ReclaimCheckingWake {
        io_core: Arc<IoCore>,
        session: Arc<SessionManager>,
        connection: Arc<ConnectionState>,
        token: OperationToken,
        observed: AtomicBool,
    }

    impl Wake for ReclaimCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.session.admission.try_write().is_ok());
            assert!(self.connection.lifecycle_unlocked_for_test());
            assert_eq!(self.io_core.operations.live(), 0);
            assert_eq!(self.connection.accepted_count(), 0);
            assert_eq!(
                self.connection
                    .io
                    .local_credit_used_for_test(Direction::Send),
                0
            );
            assert_eq!(self.io_core.cq_credits.free(), 8);
            assert_eq!(self.io_core.cq_credits.retained(), 0);
            assert!(!self.session.operation_quarantined_for_test(self.token));
            self.observed.store(true, Ordering::Release);
        }
    }

    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 18);
    connection.state.reserve_local(Direction::Send).unwrap();
    assert!(shared.io_core.cq_credits.reserve());
    let (sender, receiver) = super::super::io::event_port();
    let (token, operation) = shared
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new_with_event(
                token,
                Arc::clone(&connection.state),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
                Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
            ))
        })
        .unwrap();
    operation.commit_accepted();
    shared
        .io_core
        .accepted_operations
        .fetch_add(1, Ordering::AcqRel);
    let terminal = connection
        .state
        .mark_cm_failure(Error::ProtocolViolation("contextual close failure".into()));
    drop(terminal);
    let proof = shared
        .session
        .mint_qp_destruction_proof_for_test(&connection.state);
    let wake = Arc::new(ReclaimCheckingWake {
        io_core: Arc::clone(&shared.io_core),
        session: Arc::clone(&shared.session),
        connection: Arc::clone(&connection.state),
        token,
        observed: AtomicBool::new(false),
    });
    receiver.register(&Waker::from(Arc::clone(&wake)));

    assert!(
        shared
            .session
            .reclaim_after_qp_destroy_for_test(&proof, &connection.state, token)
    );
    assert!(wake.observed.load(Ordering::Acquire));
    let Some(super::super::io::IoEvent::Completion(completion)) = receiver.pop() else {
        panic!("QP destruction must publish an owned completion event")
    };
    let (_, _, result, _, unaccepted) = completion.into_parts();
    assert!(!unaccepted);
    assert!(matches!(
        result,
        Err(Error::ProtocolViolation(message)) if message == "contextual close failure"
    ));
}

#[test]
fn unresolved_reclamation_requires_the_exact_connection_destruction_proof() {
    let shared = synthetic_engine(8);
    let owner = synthetic_connection_on(&shared, 22);
    let other = synthetic_connection_on(&shared, 23);
    let token = install_accepted(&shared, &owner.state, WcOpcode::Send);
    let wrong_proof = shared
        .session
        .mint_qp_destruction_proof_for_test(&other.state);

    assert!(
        !shared
            .session
            .reclaim_after_qp_destroy_for_test(&wrong_proof, &owner.state, token)
    );
    assert_eq!(owner.state.accepted_count(), 1);
    assert!(matches!(
        shared.io_core.operations.lookup(token),
        Lookup::Occupied(_)
    ));

    let proof = shared
        .session
        .mint_qp_destruction_proof_for_test(&owner.state);
    assert!(
        shared
            .session
            .reclaim_after_qp_destroy_for_test(&proof, &owner.state, token)
    );
    assert_eq!(owner.state.accepted_count(), 0);
    assert_eq!(shared.io_core.operations.live(), 0);
}

#[test]
fn exact_routing_rejects_invalid_classes_and_delivers_fatal_statuses() {
    let shared = synthetic_engine(8);
    let first = synthetic_connection_on(&shared, 7);
    let exact = install_accepted(&shared, &first.state, WcOpcode::Send);
    let exact_wc = wc(exact, 7, IBV_WC_SEND);
    assert_eq!(
        shared.session.enqueue_completion(exact_wc),
        Some(first.state.token)
    );
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(first.state.token, 1),
        (1, false)
    );

    for (raw_status, expected_status) in [
        (IBV_WC_FATAL_ERR, WcStatus::FatalErr),
        (u32::MAX, WcStatus::Unknown(u32::MAX)),
    ] {
        let (fatal, events) = install_accepted_with_result(&shared, &first.state, WcOpcode::Send);
        let mut fatal_wc = wc(fatal, 7, IBV_WC_SEND);
        fatal_wc.inner.status = raw_status;
        assert_eq!(
            shared.session.enqueue_completion(fatal_wc),
            Some(first.state.token)
        );
        assert_eq!(
            shared
                .session
                .dispatch_connection_completions(first.state.token, 1),
            (1, false)
        );
        let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
            panic!("fatal completion event")
        };
        let (_, _, result, mr, unaccepted) = completion.into_parts();
        assert!(mr.is_none());
        assert!(!unaccepted);
        assert!(matches!(
            result,
            Err(Error::CompletionError { status, vendor_err: 0 })
                if status == expected_status
        ));
    }
    assert_eq!(shared.io_core.rejected_cqes.load(Ordering::Acquire), 0);

    assert!(shared.session.enqueue_completion(exact_wc).is_none());
    assert_eq!(shared.io_core.rejected_cqes.load(Ordering::Acquire), 1);

    let unknown = OperationToken {
        slot: 99,
        generation: 99,
    };
    assert!(
        shared
            .session
            .enqueue_completion(wc(unknown, 7, IBV_WC_SEND))
            .is_none()
    );

    let wrong_qp = install_accepted(&shared, &first.state, WcOpcode::Send);
    assert!(
        shared
            .session
            .enqueue_completion(wc(wrong_qp, 8, IBV_WC_SEND))
            .is_none()
    );

    let wrong_opcode = install_accepted(&shared, &first.state, WcOpcode::Send);
    assert!(
        shared
            .session
            .enqueue_completion(wc(wrong_opcode, 7, IBV_WC_RECV))
            .is_none()
    );

    let (stale, _) = shared
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(&first.state),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            ))
        })
        .unwrap();
    shared.io_core.operations.release(stale, false).unwrap();
    assert!(
        shared
            .session
            .enqueue_completion(wc(stale, 7, IBV_WC_SEND))
            .is_none()
    );

    let (retired, _) = shared
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(&first.state),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            ))
        })
        .unwrap();
    let retired = shared
        .io_core
        .operations
        .force_generation_for_test(retired, u32::MAX);
    shared.io_core.operations.release(retired, false).unwrap();
    assert!(matches!(
        shared.io_core.operations.lookup(retired),
        Lookup::Retired
    ));
    assert!(
        shared
            .session
            .enqueue_completion(wc(retired, 7, IBV_WC_SEND))
            .is_none()
    );
    assert_eq!(
        lock_unpoison(&shared.io_core.rejected_cqe_reasons).last(),
        Some(&CqeReject::RetiredOperation)
    );

    let second = synthetic_connection_on(&shared, 8);
    let wrong_connection = install_accepted(&shared, &first.state, WcOpcode::Send);
    shared
        .session
        .connections
        .set_qp_mapping_for_test(7, second.state.token);
    assert!(
        shared
            .session
            .enqueue_completion(wc(wrong_connection, 7, IBV_WC_SEND))
            .is_none()
    );

    let stale_connection = install_accepted(&shared, &second.state, WcOpcode::Send);
    shared
        .session
        .connections
        .release(second.state.token, second.state.qp_num());
    assert!(
        shared
            .session
            .enqueue_completion(wc(stale_connection, 8, IBV_WC_SEND))
            .is_none()
    );

    assert_eq!(shared.io_core.rejected_cqes.load(Ordering::Acquire), 8);
    let rejection_reasons = lock_unpoison(&shared.io_core.rejected_cqe_reasons);
    assert!(rejection_reasons.contains(&CqeReject::StaleOperation));
    assert!(rejection_reasons.contains(&CqeReject::RetiredOperation));
}

#[test]
fn queued_completion_marker_rejects_duplicates_before_dispatch() {
    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 16);
    let token = install_accepted(&shared, &connection.state, WcOpcode::Send);
    let completion = wc(token, 16, IBV_WC_SEND);

    assert_eq!(
        shared.session.enqueue_completion(completion),
        Some(connection.state.token)
    );
    assert!(shared.session.enqueue_completion(completion).is_none());
    assert_eq!(shared.io_core.rejected_cqes.load(Ordering::Acquire), 1);
    assert_eq!(
        lock_unpoison(&shared.io_core.rejected_cqe_reasons).as_slice(),
        &[CqeReject::Duplicate]
    );
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 2),
        (1, false)
    );
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        0
    );
    assert_eq!(shared.io_core.cq_credits.free(), 8);
}

#[test]
fn duplicate_connection_installation_releases_new_slot_without_replacing_qp_index() {
    let shared = synthetic_engine(8);
    let first = synthetic_connection_on(&shared, 9);
    let duplicate = install_connection(
        &shared.session,
        Arc::new(NoopPoster(9)),
        super::super::RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
    );
    assert!(matches!(duplicate, Err(Error::InvalidConfig(_))));
    assert_eq!(shared.session.connections.live(), 1);
    assert_eq!(shared.session.connections.free(), 7);
    assert_eq!(
        shared.session.connections.lookup_qp(9),
        Some(first.state.token),
        "the original exact qp_num mapping must remain installed"
    );
}

#[test]
fn completion_dispatch_budget_bounds_routed_work_without_idle_scans() {
    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 17);
    for _ in 0..3 {
        let token = install_accepted(&shared, &connection.state, WcOpcode::Recv);
        assert_eq!(
            shared
                .session
                .enqueue_completion(wc(token, 17, IBV_WC_RECV)),
            Some(connection.state.token)
        );
    }
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 2),
        (2, true)
    );
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 2),
        (1, false)
    );
}

#[test]
fn batch_ownership_transfers_each_prefix_and_ambiguous_batch_once() {
    for first_unaccepted in 0..=4 {
        let transfer = PreparedBatchOwnership::new(vec![0, 1, 2, 3])
            .unwrap()
            .consume(BatchPostOutcome::PrefixAccepted {
                accepted: first_unaccepted,
                first_unaccepted,
                source: std::io::Error::from_raw_os_error(libc::ENOMEM),
            });
        let BatchOwnershipTransfer::Partial {
            accepted,
            unaccepted,
            ..
        } = transfer
        else {
            panic!("valid bad_wr member must split the ledger")
        };
        assert_eq!(accepted, (0..first_unaccepted).collect::<Vec<_>>());
        assert_eq!(unaccepted, (first_unaccepted..4).collect::<Vec<_>>());
    }

    let transfer =
        PreparedBatchOwnership::new(vec![1, 2, 3])
            .unwrap()
            .consume(BatchPostOutcome::Ambiguous {
                source: std::io::Error::from_raw_os_error(libc::EIO),
            });
    let BatchOwnershipTransfer::Ambiguous { retained, .. } = transfer else {
        panic!("ambiguous bad_wr must retain the complete batch")
    };
    assert_eq!(retained, vec![1, 2, 3]);
}

#[test]
fn validation_failure_is_a_zero_call_full_rollback() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        41,
        ScriptedPost::Accepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 2, 2);
    let mr = connection
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let mut operation = connection.send(mr, Some((63, 2)));
    let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
        panic!("invalid range must fail synchronously")
    };
    assert!(matches!(result, Err(Error::InvalidConfig(_))));
    assert!(returned.is_some());
    assert_eq!(poster.calls(), 0);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    assert_eq!(connection.state.accepted_count(), 0);

    let mut operation = connection.send(returned.unwrap(), None);
    assert!(poll_once(&mut operation).is_pending());
    let token = poster.tokens()[0];
    complete(&shared.session, &connection.state, token, IBV_WC_SEND);
    drop(operation);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn cancellation_before_first_poll_posts_nothing_and_releases_unregistered_mr() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        50,
        ScriptedPost::Accepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let recorder = DestructionRecorder::arm(4);
    let operation = connection.send(
        connection
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap(),
        None,
    );
    drop(operation);
    assert_eq!(poster.calls(), 0);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        1
    );
    drop(connection);
    drop(driver);
    drop(engine);
    drop(recorder);
}

#[test]
fn local_direction_exhaustion_posts_nothing_and_preserves_global_capacity() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        42,
        ScriptedPost::Accepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let first_mr = connection
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let second_mr = connection
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let mut first = connection.send(first_mr, None);
    assert!(poll_once(&mut first).is_pending());
    let mut second = connection.send(second_mr, None);
    let Poll::Ready((result, returned)) = poll_once(&mut second) else {
        panic!("local exhaustion must be synchronous")
    };
    assert!(matches!(result, Err(Error::CapacityExhausted)));
    assert!(returned.is_some());
    assert_eq!(poster.calls(), 1);
    assert_eq!(shared.io_core.operations.live(), 1);
    assert_eq!(shared.io_core.cq_credits.free(), 3);
    assert_eq!(connection.state.accepted_count(), 1);

    complete(
        &shared.session,
        &connection.state,
        poster.tokens()[0],
        IBV_WC_SEND,
    );
    drop(first);
    drop(returned);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn operation_global_exhaustion_precedes_and_preserves_the_cq_invariant() {
    let Some((engine, driver, shared)) = production_engine(3, 2, 2) else {
        return;
    };
    let first_poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        43,
        ScriptedPost::Accepted,
    ));
    let second_poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        44,
        ScriptedPost::Accepted,
    ));
    let first = scripted_connection(&shared.session, Arc::clone(&first_poster), 1, 1);
    let second = scripted_connection(&shared.session, Arc::clone(&second_poster), 1, 1);

    let mut send = first.send(
        first.register_memory(64, AccessIntent::LocalOnly).unwrap(),
        None,
    );
    let mut recv = first.recv(
        first.register_memory(64, AccessIntent::LocalOnly).unwrap(),
        None,
    );
    assert!(poll_once(&mut send).is_pending());
    assert!(poll_once(&mut recv).is_pending());
    let mut rejected = second.send(
        second.register_memory(64, AccessIntent::LocalOnly).unwrap(),
        None,
    );
    let Poll::Ready((result, returned)) = poll_once(&mut rejected) else {
        panic!("global operation exhaustion must be synchronous")
    };
    assert!(matches!(result, Err(Error::CapacityExhausted)));
    assert!(returned.is_some());
    assert_eq!(second_poster.calls(), 0);
    assert_eq!(shared.io_core.operations.live(), 2);
    assert_eq!(shared.io_core.cq_credits.free(), 0);
    second
        .state
        .reserve_local(Direction::Send)
        .expect("global rejection must restore the connection-local credit");
    second.state.release_local(Direction::Send);

    let tokens = first_poster.tokens();
    complete(&shared.session, &first.state, tokens[0], IBV_WC_SEND);
    complete(&shared.session, &first.state, tokens[1], IBV_WC_RECV);
    drop(send);
    drop(recv);
    drop(returned);
    drop(first);
    drop(second);
    drop(driver);
    drop(engine);
}

#[test]
fn wholly_unaccepted_post_restores_mr_slot_local_and_cq_reservations() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        45,
        ScriptedPost::Unaccepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let mut operation = connection.send(
        connection
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap(),
        None,
    );
    let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
        panic!("provider-proven rejection must return immediately")
    };
    assert!(matches!(result, Err(Error::PostFailed(_))));
    assert!(returned.is_some());
    assert_eq!(poster.calls(), 1);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    assert_eq!(connection.state.accepted_count(), 0);
    connection
        .state
        .reserve_local(Direction::Send)
        .expect("proven-unaccepted rollback must restore local credit");
    connection.state.release_local(Direction::Send);
    drop(returned);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn empty_and_zero_accepted_io_batches_reject_and_roll_back_every_reservation() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        51,
        ScriptedPost::Unaccepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 2);
    let (io, events) =
        super::super::io::IoConnection::new(&shared.session, Arc::clone(&connection.state))
            .unwrap();

    assert!(matches!(
        io.post_recv_batch(Vec::new()),
        IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: 0,
            error: Error::InvalidConfig(_)
        }
    ));
    assert_eq!(poster.calls(), 0);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 4);

    let recorder = DestructionRecorder::arm(8);
    let mut entries = Vec::new();
    for _ in 0..2 {
        let mr = connection
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        entries.push(IoRecvRequest::new(mr, IoOperationContext::new(())));
    }
    assert!(matches!(
        io.post_recv_batch(entries),
        IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: 2,
            error: Error::PostFailed(_)
        }
    ));
    assert_eq!(events.queued_len(), 2);
    assert_eq!(poster.calls(), 1);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    assert_eq!(connection.state.accepted_count(), 0);
    connection.state.reserve_local(Direction::Recv).unwrap();
    connection.state.reserve_local(Direction::Recv).unwrap();
    connection.state.release_local(Direction::Recv);
    connection.state.release_local(Direction::Recv);
    drop(events.drain());
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        2
    );

    drop(connection);
    drop(driver);
    drop(engine);
    drop(recorder);
}

#[test]
fn exact_prefix_returns_only_the_proven_unaccepted_suffix() {
    let Some((engine, driver, shared)) = production_engine(2, 8, 8) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        53,
        ScriptedPost::PrefixAccepted(1),
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 3);
    let (io, events) =
        super::super::io::IoConnection::new(&shared.session, Arc::clone(&connection.state))
            .unwrap();
    let requests = (0usize..3)
        .map(|context| {
            IoRecvRequest::new(
                io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
                IoOperationContext::new(context),
            )
        })
        .collect();

    assert!(matches!(
        io.post_recv_batch(requests),
        IoSubmissionDisposition::ExactPrefix {
            accepted: 1,
            proven_unaccepted: 2,
            error: Error::PostFailed(_)
        }
    ));
    let mut rejected = Vec::new();
    for _ in 0..2 {
        let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
            panic!("proven-unaccepted suffix event")
        };
        let (_, context, result, mr, unaccepted) = completion.into_parts();
        rejected.push(context.downcast::<usize>().ok().unwrap());
        assert!(matches!(result, Err(Error::PostFailed(_))));
        assert!(mr.is_some());
        assert!(unaccepted);
    }
    rejected.sort_unstable();
    assert_eq!(rejected, vec![1, 2]);
    assert_eq!(connection.state.accepted_count(), 1);
    complete(
        &shared.session,
        &connection.state,
        poster.tokens()[0],
        IBV_WC_RECV,
    );
    assert!(matches!(
        events.pop(),
        Some(super::super::io::IoEvent::Completion(_))
    ));
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 8);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn exact_prefix_with_early_suffix_cqe_retains_the_entire_batch() {
    let Some((engine, driver, shared)) = production_engine(2, 8, 8) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        54,
        ScriptedPost::PrefixWithSuffixCompletion {
            accepted: 1,
            completed_suffix: 1,
        },
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 3);
    let (io, events) =
        super::super::io::IoConnection::new(&shared.session, Arc::clone(&connection.state))
            .unwrap();
    let requests = (0usize..3)
        .map(|context| {
            IoRecvRequest::new(
                io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
                IoOperationContext::new(context),
            )
        })
        .collect();

    assert!(matches!(
        io.post_recv_batch(requests),
        IoSubmissionDisposition::RetainedAfterEarlyCompletion {
            retained: 3,
            error: Error::PostFailed(_)
        }
    ));
    let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
        panic!("early suffix completion event")
    };
    let (_, context, result, mr, unaccepted) = completion.into_parts();
    assert_eq!(context.downcast::<usize>().ok().unwrap(), 1);
    assert!(result.is_ok());
    assert!(mr.is_some());
    assert!(!unaccepted);
    assert!(!events.has_events());
    assert_eq!(connection.state.accepted_count(), 2);
    assert_eq!(shared.io_core.operations.live(), 2);

    let tokens = poster.tokens();
    complete(&shared.session, &connection.state, tokens[0], IBV_WC_RECV);
    complete(&shared.session, &connection.state, tokens[2], IBV_WC_RECV);
    assert_eq!(events.drain().len(), 2);
    assert_eq!(connection.state.accepted_count(), 0);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 8);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn exact_prefix_with_queued_suffix_cqe_retains_the_entire_batch_until_dispatch() {
    let Some((engine, driver, shared)) = production_engine(2, 8, 8) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        55,
        ScriptedPost::PrefixWithQueuedSuffixCompletion {
            accepted: 1,
            completed_suffix: 2,
        },
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 3);
    let (io, events) =
        super::super::io::IoConnection::new(&shared.session, Arc::clone(&connection.state))
            .unwrap();
    let requests = (0usize..3)
        .map(|context| {
            IoRecvRequest::new(
                io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
                IoOperationContext::new(context),
            )
        })
        .collect();

    assert!(matches!(
        io.post_recv_batch(requests),
        IoSubmissionDisposition::RetainedAfterEarlyCompletion {
            retained: 3,
            error: Error::PostFailed(_)
        }
    ));
    assert!(!events.has_events());
    assert_eq!(connection.state.accepted_count(), 3);
    assert_eq!(shared.io_core.operations.live(), 3);
    assert_eq!(shared.io_core.cq_credits.free(), 5);

    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 1),
        (1, false)
    );
    let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
        panic!("queued suffix completion event")
    };
    let (_, context, result, mr, unaccepted) = completion.into_parts();
    assert_eq!(context.downcast::<usize>().ok().unwrap(), 2);
    assert!(result.is_ok());
    assert!(mr.is_some());
    assert!(!unaccepted);
    assert_eq!(connection.state.accepted_count(), 2);
    assert_eq!(shared.io_core.operations.live(), 2);

    let tokens = poster.tokens();
    complete(&shared.session, &connection.state, tokens[0], IBV_WC_RECV);
    complete(&shared.session, &connection.state, tokens[1], IBV_WC_RECV);
    assert_eq!(events.drain().len(), 2);
    assert_eq!(connection.state.accepted_count(), 0);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 8);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn dispatch_between_releasability_observation_and_release_retains_the_whole_suffix() {
    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 56);
    let mut entries = Vec::new();
    for _ in 0..2 {
        connection.state.reserve_local(Direction::Recv).unwrap();
        assert!(shared.io_core.cq_credits.reserve());
        let (token, state) = shared
            .io_core
            .operations
            .allocate(|token| {
                Arc::new(OperationState::new(
                    token,
                    Arc::clone(&connection.state),
                    Direction::Recv,
                    WcOpcode::Recv,
                    None,
                    1,
                ))
            })
            .unwrap();
        entries.push(InternalBatchEntry {
            token,
            state,
            sge: Sge::new(0, 0, 0),
        });
    }
    assert!(
        entries
            .iter()
            .all(|entry| entry.state.can_release_unaccepted_for_test())
    );

    let raced = entries[1].token;
    assert_eq!(
        shared
            .session
            .enqueue_completion(wc(raced, 56, IBV_WC_RECV)),
        Some(connection.state.token)
    );
    assert_eq!(entries[1].state.completion_ownership_for_test(), "queued");
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 1),
        (1, false)
    );
    assert_eq!(entries[1].state.completion_ownership_for_test(), "early");

    let entries = match release_proven_unaccepted_entries(
        &shared.io_core,
        &connection.state,
        Direction::Recv,
        entries,
        Error::PostFailed(std::io::Error::from_raw_os_error(libc::ENOMEM)),
    ) {
        InternalRelease::Retained(entries) => entries,
        InternalRelease::Released(after_unlock) => {
            // Bind and drop the payload rather than ignoring it: this arm is the
            // only reader of the mirrored `Released` variant, so an `_` pattern
            // would make the field look dead, and dropping before the panic
            // keeps the assertion message intact.
            drop(after_unlock);
            panic!("a recorded suffix CQE must prevent every suffix release")
        }
    };
    assert_eq!(shared.io_core.operations.live(), 2);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        0
    );
    assert_eq!(shared.io_core.cq_credits.free(), 6);
    connection.state.reserve_local(Direction::Recv).unwrap();
    connection.state.reserve_local(Direction::Recv).unwrap();
    assert!(matches!(
        connection.state.reserve_local(Direction::Recv),
        Err(Error::CapacityExhausted)
    ));
    connection.state.release_local(Direction::Recv);
    connection.state.release_local(Direction::Recv);

    commit_internal_entries(&shared.io_core, entries).publish();
    assert_eq!(shared.io_core.operations.live(), 1);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        1
    );
    assert_eq!(connection.state.accepted_count(), 1);
    assert_eq!(shared.io_core.cq_credits.free(), 7);

    let remaining = connection.state.accepted_tokens();
    assert_eq!(remaining.len(), 1);
    complete(
        &shared.session,
        &connection.state,
        remaining[0],
        IBV_WC_RECV,
    );
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        0
    );
    assert_eq!(connection.state.accepted_count(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 8);
    for _ in 0..4 {
        connection.state.reserve_local(Direction::Recv).unwrap();
    }
    assert!(matches!(
        connection.state.reserve_local(Direction::Recv),
        Err(Error::CapacityExhausted)
    ));
    for _ in 0..4 {
        connection.state.release_local(Direction::Recv);
    }
}

#[test]
fn ambiguous_acceptance_retains_mr_identity_slot_and_cq_until_exact_dispatch() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        46,
        ScriptedPost::Ambiguous,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let mr = connection
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let recorder = DestructionRecorder::arm(8);
    let mut operation = connection.send(mr, None);
    let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
        panic!("ambiguous post reports its contextual error immediately")
    };
    assert!(matches!(result, Err(Error::PostFailed(_))));
    assert!(returned.is_none());
    assert_eq!(shared.io_core.operations.live(), 1);
    assert_eq!(shared.io_core.cq_credits.free(), 3);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        1
    );
    assert_eq!(
        shared.io_core.pending_reclamations.load(Ordering::Acquire),
        1
    );
    assert_eq!(connection.state.accepted_count(), 1);
    assert!(recorder.snapshot().is_empty());
    complete(
        &shared.session,
        &connection.state,
        poster.tokens()[0],
        IBV_WC_SEND,
    );
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    assert_eq!(
        shared.io_core.pending_reclamations.load(Ordering::Acquire),
        0
    );
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        1
    );
    drop(operation);
    drop(connection);
    drop(driver);
    drop(engine);
    drop(recorder);
}

#[test]
fn completion_dispatched_during_post_commits_and_releases_exactly_once() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        47,
        ScriptedPost::DispatchDuringPost,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let mut operation = connection.send(
        connection
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap(),
        None,
    );
    let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
        panic!("the early exact CQE must be delivered by the first poll")
    };
    result.unwrap();
    assert!(returned.is_some());
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        0
    );
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    drop(returned);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn io_early_completion_event_is_published_after_post_guards_are_released() {
    use std::task::{Wake, Waker};

    struct PostGuardCheckingWake {
        admission: Arc<RwLock<()>>,
        connection: Arc<ConnectionState>,
        observed: AtomicBool,
    }

    impl Wake for PostGuardCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.admission.try_write().is_ok());
            assert!(
                self.connection.io.posting_write_unlocked_for_test(),
                "early-completion publication must release the posting read guard"
            );
            self.observed.store(true, Ordering::Release);
        }
    }

    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        49,
        ScriptedPost::DispatchDuringPost,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let (io, events) =
        super::super::io::IoConnection::new(&shared.session, Arc::clone(&connection.state))
            .unwrap();
    let wake = Arc::new(PostGuardCheckingWake {
        admission: Arc::clone(&shared.session.admission),
        connection: Arc::clone(&connection.state),
        observed: AtomicBool::new(false),
    });
    events.register(&Waker::from(Arc::clone(&wake)));
    let posted = io.post_send(IoSendRequest::new(
        io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
        1,
        IoOperationContext::new(()),
    ));
    assert!(posted.all_accepted(), "I/O early-completion post failed");
    assert!(wake.observed.load(Ordering::Acquire));
    let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
        panic!("early completion event")
    };
    let (_, _, result, mr, unaccepted) = completion.into_parts();
    assert!(result.is_ok());
    assert!(mr.is_some());
    assert!(!unaccepted);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        0
    );
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn scalar_early_publication_helper_releases_post_guards_without_a_provider() {
    use std::task::{Wake, Waker};

    struct PostGuardCheckingWake {
        admission: Arc<RwLock<()>>,
        connection: Arc<ConnectionState>,
        observed: AtomicBool,
    }

    impl Wake for PostGuardCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.admission.try_write().is_ok());
            assert!(self.connection.io.posting_write_unlocked_for_test());
            self.observed.store(true, Ordering::Release);
        }
    }

    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 55);
    let token = install_accepted(&shared, &connection.state, WcOpcode::Send);
    let Lookup::Occupied(operation) = shared.io_core.operations.lookup(token) else {
        panic!("accepted operation")
    };
    let wake = Arc::new(PostGuardCheckingWake {
        admission: Arc::clone(&shared.session.admission),
        connection: Arc::clone(&connection.state),
        observed: AtomicBool::new(false),
    });
    operation.register_waker(&Waker::from(Arc::clone(&wake)));

    let admission = shared.io_core.admission();
    let posting = connection.state.io.begin_posting().unwrap();
    let mut after_unlock = AfterEngineUnlock::default();
    after_unlock.push_operation_wake(operation);
    publish_after_post_guards(posting, admission, after_unlock);

    assert!(wake.observed.load(Ordering::Acquire));
}

#[test]
fn io_unaccepted_event_is_published_after_post_guards_are_released() {
    use std::task::{Wake, Waker};

    struct PostGuardCheckingWake {
        admission: Arc<RwLock<()>>,
        connection: Arc<ConnectionState>,
        observed: AtomicBool,
    }

    impl Wake for PostGuardCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.admission.try_write().is_ok());
            assert!(
                self.connection.io.posting_write_unlocked_for_test(),
                "unaccepted publication must release the posting read guard"
            );
            self.observed.store(true, Ordering::Release);
        }
    }

    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        52,
        ScriptedPost::Unaccepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let (io, events) =
        super::super::io::IoConnection::new(&shared.session, Arc::clone(&connection.state))
            .unwrap();
    let wake = Arc::new(PostGuardCheckingWake {
        admission: Arc::clone(&shared.session.admission),
        connection: Arc::clone(&connection.state),
        observed: AtomicBool::new(false),
    });
    events.register(&Waker::from(Arc::clone(&wake)));
    let posted = io.post_send(IoSendRequest::new(
        io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
        1,
        IoOperationContext::new(()),
    ));
    assert!(matches!(
        posted,
        IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: 1,
            error: Error::PostFailed(_)
        }
    ));
    assert!(wake.observed.load(Ordering::Acquire));
    let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
        panic!("unaccepted event")
    };
    let (_, _, result, mr, unaccepted) = completion.into_parts();
    assert!(matches!(result, Err(Error::PostFailed(_))));
    assert!(mr.is_some());
    assert!(unaccepted);
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(
        shared.io_core.accepted_operations.load(Ordering::Acquire),
        0
    );
    assert_eq!(shared.io_core.cq_credits.free(), 4);
    drop(connection);
    drop(driver);
    drop(engine);
}

#[test]
fn accepted_zero_is_committed_before_completion_event_publication() {
    use std::task::{Wake, Waker};

    struct DrainCheckingWake {
        admission: Arc<RwLock<()>>,
        connection: Arc<ConnectionState>,
        observed: AtomicBool,
    }

    impl Wake for DrainCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.admission.try_write().is_ok());
            assert!(
                self.connection.drained_and_retirement_requested_for_test(),
                "accepted-zero session effects must commit before event publication"
            );
            self.observed.store(true, Ordering::Release);
        }
    }

    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 53);
    connection.state.reserve_local(Direction::Send).unwrap();
    assert!(shared.io_core.cq_credits.reserve());
    let (sender, events) = super::super::io::event_port();
    let (token, operation) = shared
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new_with_event(
                token,
                Arc::clone(&connection.state),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
                Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
            ))
        })
        .unwrap();
    connection.state.add_accepted(token);
    operation.commit_accepted();
    shared
        .io_core
        .accepted_operations
        .fetch_add(1, Ordering::AcqRel);
    shared.session.begin_connection_close(&connection.state);

    let wake = Arc::new(DrainCheckingWake {
        admission: Arc::clone(&shared.session.admission),
        connection: Arc::clone(&connection.state),
        observed: AtomicBool::new(false),
    });
    events.register(&Waker::from(Arc::clone(&wake)));
    assert_eq!(
        shared
            .session
            .enqueue_completion(wc(token, 53, IBV_WC_SEND)),
        Some(connection.state.token)
    );
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 1),
        (1, false)
    );

    assert!(wake.observed.load(Ordering::Acquire));
    assert!(connection.state.drained_and_retirement_requested_for_test());
    let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
        panic!("completion event")
    };
    assert!(completion.into_parts().2.is_ok());
}

#[test]
fn quarantine_clear_is_committed_before_completion_event_publication() {
    use std::task::{Wake, Waker};

    struct QuarantineCheckingWake {
        session: Arc<SessionManager>,
        token: OperationToken,
        observed: AtomicBool,
    }

    impl Wake for QuarantineCheckingWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(
                !self.session.operation_quarantined_for_test(self.token),
                "quarantine clear must commit before event publication"
            );
            self.observed.store(true, Ordering::Release);
        }
    }

    let shared = synthetic_engine(8);
    let connection = synthetic_connection_on(&shared, 54);
    connection.state.reserve_local(Direction::Send).unwrap();
    assert!(shared.io_core.cq_credits.reserve());
    let (sender, events) = super::super::io::event_port();
    let (token, operation) = shared
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new_with_event(
                token,
                Arc::clone(&connection.state),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
                Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
            ))
        })
        .unwrap();
    connection.state.add_accepted(token);
    operation.commit_accepted();
    shared
        .io_core
        .accepted_operations
        .fetch_add(1, Ordering::AcqRel);
    shared.session.quarantine_operation(token);
    assert!(shared.session.operation_quarantined_for_test(token));

    let wake = Arc::new(QuarantineCheckingWake {
        session: Arc::clone(&shared.session),
        token,
        observed: AtomicBool::new(false),
    });
    events.register(&Waker::from(Arc::clone(&wake)));
    assert_eq!(
        shared
            .session
            .enqueue_completion(wc(token, 54, IBV_WC_SEND)),
        Some(connection.state.token)
    );
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 1),
        (1, false)
    );

    assert!(wake.observed.load(Ordering::Acquire));
    assert!(!shared.session.operation_quarantined_for_test(token));
    let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
        panic!("completion event")
    };
    assert!(completion.into_parts().2.is_ok());
}

#[test]
fn cancellation_and_dispatch_race_releases_each_mr_and_reservation_once() {
    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        48,
        ScriptedPost::Accepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let recorder = DestructionRecorder::arm(64);

    for iteration in 0..32 {
        let mut operation = connection.send(
            connection
                .register_memory(64, AccessIntent::LocalOnly)
                .unwrap(),
            None,
        );
        assert!(poll_once(&mut operation).is_pending());
        let token = poster.tokens()[iteration];
        let barrier = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            let drop_barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                drop_barrier.wait();
                drop(operation);
            });
            let dispatch_barrier = Arc::clone(&barrier);
            let session = Arc::clone(&shared.session);
            let connection = Arc::clone(&connection.state);
            scope.spawn(move || {
                dispatch_barrier.wait();
                complete(&session, &connection, token, IBV_WC_SEND);
            });
            barrier.wait();
        });
        assert_eq!(shared.io_core.operations.live(), 0);
        assert_eq!(
            shared.io_core.accepted_operations.load(Ordering::Acquire),
            0
        );
        assert_eq!(
            shared.io_core.pending_reclamations.load(Ordering::Acquire),
            0
        );
        assert_eq!(shared.io_core.cq_credits.free(), 4);
    }
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        32
    );
    drop(connection);
    drop(driver);
    drop(engine);
    drop(recorder);
}

#[tokio::test]
async fn driver_drop_wakes_all_waiters_and_retains_accepted_mrs_fail_closed() {
    use futures_util::task::{ArcWake, waker};

    struct WakeCounter(AtomicUsize);
    impl ArcWake for WakeCounter {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    let Some((engine, driver, shared)) = production_engine(2, 4, 4) else {
        return;
    };
    let poster = Arc::new(ScriptedPoster::new(
        &shared.session,
        49,
        ScriptedPost::Accepted,
    ));
    let connection = scripted_connection(&shared.session, Arc::clone(&poster), 1, 1);
    let send_mr = connection
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let recv_mr = connection
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let rejected_mr = connection
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let recorder = DestructionRecorder::arm(16);
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = waker(Arc::clone(&counter));
    let mut cx = Context::from_waker(&waker);

    let mut send = Box::pin(connection.send(send_mr, None));
    let mut recv = Box::pin(connection.recv(recv_mr, None));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(recv.as_mut().poll(&mut cx).is_pending());
    let mut shutdown = Box::pin(engine.shutdown());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    let mut rejected = connection.send(rejected_mr, None);
    let Poll::Ready((result, returned)) = poll_once(&mut rejected) else {
        panic!("shutdown admission barrier must reject without posting")
    };
    assert!(matches!(result, Err(Error::DriverShutdown)));
    drop(returned);

    let mut close = Box::pin(connection.close());
    assert!(close.as_mut().poll(&mut cx).is_pending());
    drop(driver);
    assert!(
        counter.0.load(Ordering::Acquire) >= 4,
        "two operations, connection close, and shutdown must all be woken"
    );
    for operation in [&mut send, &mut recv] {
        let Poll::Ready((result, returned)) = operation.as_mut().poll(&mut cx) else {
            panic!("every in-flight operation must resolve after driver drop")
        };
        assert!(matches!(
            result,
            Err(Error::EngineWedged {
                outstanding_operations: 2,
                ..
            })
        ));
        assert!(returned.is_none());
    }
    assert!(matches!(
        close.as_mut().poll(&mut cx),
        Poll::Ready(Err(Error::EngineWedged {
            outstanding_operations: 2,
            ..
        }))
    ));
    assert!(matches!(
        shutdown.as_mut().poll(&mut cx),
        Poll::Ready(Err(Error::EngineWedged {
            outstanding_operations: 2,
            ..
        }))
    ));
    assert_eq!(poster.error_transitions(), 1);
    let diagnostics = engine.diagnostics();
    assert_eq!(
        diagnostics.lifecycle,
        super::super::RdmaEngineLifecycle::Failed
    );
    assert_eq!(diagnostics.quarantined_operations, 2);
    assert_eq!(diagnostics.quarantined_mrs, 2);
    assert_eq!(diagnostics.retained_cq_credits, 2);
    let released_mrs = recorder
        .snapshot()
        .iter()
        .filter(|event| event.kind == DestructionKind::MemoryRegion)
        .count();
    assert_eq!(released_mrs, 1, "only the proven-unposted MR is released");

    drop(send);
    drop(recv);
    drop(close);
    drop(shutdown);
    drop(connection);
    drop(engine);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        1,
        "accepted MRs remain in process-lifetime fail-closed ownership"
    );
    drop(recorder);
}

#[tokio::test]
async fn terminal_wakers_can_reenter_after_terminal_guards_drop() {
    use futures_util::task::{ArcWake, waker};

    struct ReentrantWaker {
        shared: Arc<EngineShared>,
        connection: Arc<ConnectionState>,
        token: OperationToken,
        label: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
        wakes: AtomicUsize,
        lock_failures: AtomicUsize,
    }

    impl ArcWake for ReentrantWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.wakes.fetch_add(1, Ordering::AcqRel);
            let admission_unlocked = arc_self.shared.session.admission.try_write().is_ok();
            let terminal_unlocked = arc_self.shared.terminal.try_lock().is_ok();
            let quarantine_committed = arc_self
                .shared
                .session
                .operation_quarantined_for_test(arc_self.token);
            let operation_terminal = matches!(
                arc_self.shared.io_core.operations.lookup(arc_self.token),
                Lookup::Occupied(operation)
                    if operation.lifecycle() == OperationLifecycle::Quarantined
            );
            let connection_terminal = arc_self.connection.close_state().raw_outcome().is_some();
            if admission_unlocked
                && terminal_unlocked
                && quarantine_committed
                && operation_terminal
                && connection_terminal
            {
                let _ = arc_self.shared.diagnostics();
                lock_unpoison(&arc_self.order).push(arc_self.label);
            } else {
                arc_self.lock_failures.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    let shared = synthetic_engine_root(8);
    let owners = OperationOwners {
        io_core: Arc::clone(&shared.io_core),
        session: Arc::clone(&shared.session),
        _runtime: shared.clone(),
    };
    let connection = synthetic_connection_on(&owners, 50);
    let token = install_accepted(&owners, &connection.state, WcOpcode::Send);
    let Lookup::Occupied(operation) = shared.io_core.operations.lookup(token) else {
        panic!("accepted operation")
    };
    let order = Arc::new(Mutex::new(Vec::new()));
    let observer = |label| {
        Arc::new(ReentrantWaker {
            shared: Arc::clone(&shared),
            connection: Arc::clone(&connection.state),
            token,
            label,
            order: Arc::clone(&order),
            wakes: AtomicUsize::new(0),
            lock_failures: AtomicUsize::new(0),
        })
    };
    let connection_event = observer("connection-event");
    let operation_wake = observer("operation");
    let close_wake = observer("close");
    let terminal_wake = observer("terminal");

    let (_io, events) =
        super::super::io::IoConnection::new(&shared.session, Arc::clone(&connection.state))
            .unwrap();
    events.register(&waker(Arc::clone(&connection_event)));
    operation.register_waker(&waker(Arc::clone(&operation_wake)));
    let close_notify = connection.state.close_state().notify();
    let mut close_notified = Box::pin(close_notify.notified());
    assert!(
        close_notified
            .as_mut()
            .poll(&mut Context::from_waker(&waker(Arc::clone(&close_wake))))
            .is_pending()
    );
    let mut terminal_notified = Box::pin(shared.terminal_notify.notified());
    assert!(
        terminal_notified
            .as_mut()
            .poll(&mut Context::from_waker(&waker(Arc::clone(&terminal_wake))))
            .is_pending()
    );

    shared.finish(MemoizedTerminalResult::from_error(Error::EngineWedged {
        retained_bundles: 1,
        outstanding_operations: 1,
        cq_debt: 1,
    }));

    assert_eq!(
        lock_unpoison(&order).as_slice(),
        ["connection-event", "operation", "close", "terminal"],
        "root terminal publication order must remain deterministic"
    );
    for reentrant in [
        &connection_event,
        &operation_wake,
        &close_wake,
        &terminal_wake,
    ] {
        assert_eq!(reentrant.wakes.load(Ordering::Acquire), 1);
        assert_eq!(
            reentrant.lock_failures.load(Ordering::Acquire),
            0,
            "terminal observers must run after session mutation and guard release"
        );
    }
    assert_eq!(
        shared
            .io_core
            .quarantined_operations
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(shared.io_core.quarantined_mrs.load(Ordering::Acquire), 1);
    assert_eq!(shared.io_core.cq_credits.retained(), 1);
}

#[tokio::test(start_paused = true)]
async fn cancelled_operation_deadline_retains_slot_mr_debt_and_late_routing() {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Context;
    use std::time::Duration;

    let shared = synthetic_engine_root(8);
    let owners = OperationOwners {
        io_core: Arc::clone(&shared.io_core),
        session: Arc::clone(&shared.session),
        _runtime: shared.clone(),
    };
    let connection = synthetic_connection_on(&owners, 27);
    let token = install_accepted(&owners, &connection.state, WcOpcode::Recv);
    let Lookup::Occupied(operation) = shared.io_core.operations.lookup(token) else {
        panic!("accepted operation")
    };
    assert!(operation.cancel(&shared.io_core));
    shared.io_core.schedule_reclamation(token);

    let mut driver = super::super::RdmaEngineDriver::new(Arc::clone(&shared), None);
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());

    assert_eq!(shared.io_core.operations.live(), 1);
    assert_eq!(shared.io_core.cq_credits.free(), 7);
    assert_eq!(shared.io_core.cq_credits.retained(), 1);
    assert_eq!(
        shared.io_core.pending_reclamations.load(Ordering::Acquire),
        0
    );
    assert_eq!(
        shared
            .io_core
            .quarantined_operations
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(shared.io_core.quarantined_mrs.load(Ordering::Acquire), 1);
    assert_eq!(shared.io_core.quarantined_bytes.load(Ordering::Acquire), 1);

    let completion = wc(token, 27, IBV_WC_RECV);
    assert_eq!(
        shared.session.enqueue_completion(completion),
        Some(connection.state.token)
    );
    assert_eq!(
        shared
            .session
            .dispatch_connection_completions(connection.state.token, 1),
        (1, false)
    );
    assert_eq!(shared.io_core.operations.live(), 0);
    assert_eq!(shared.io_core.cq_credits.free(), 8);
    assert_eq!(shared.io_core.cq_credits.retained(), 0);
    assert_eq!(
        shared.io_core.pending_reclamations.load(Ordering::Acquire),
        0
    );
    assert_eq!(
        shared
            .io_core
            .quarantined_operations
            .load(Ordering::Acquire),
        0
    );
    assert_eq!(shared.io_core.quarantined_mrs.load(Ordering::Acquire), 0);
    assert_eq!(shared.io_core.quarantined_bytes.load(Ordering::Acquire), 0);
}

#[derive(Clone, Copy)]
enum ScriptedPost {
    Accepted,
    Unaccepted,
    Ambiguous,
    DispatchDuringPost,
    PrefixAccepted(usize),
    PrefixWithQueuedSuffixCompletion {
        accepted: usize,
        completed_suffix: usize,
    },
    PrefixWithSuffixCompletion {
        accepted: usize,
        completed_suffix: usize,
    },
}

struct ScriptedPoster {
    session: Weak<SessionManager>,
    qp_num: u32,
    outcome: ScriptedPost,
    calls: AtomicUsize,
    error_transitions: AtomicUsize,
    tokens: Mutex<Vec<OperationToken>>,
}

impl ScriptedPoster {
    fn new(session: &Arc<SessionManager>, qp_num: u32, outcome: ScriptedPost) -> Self {
        Self {
            session: Arc::downgrade(session),
            qp_num,
            outcome,
            calls: AtomicUsize::new(0),
            error_transitions: AtomicUsize::new(0),
            tokens: Mutex::new(Vec::new()),
        }
    }

    fn post(&self, tokens: Vec<OperationToken>, opcode: u32) -> BatchPostOutcome {
        self.calls.fetch_add(1, Ordering::AcqRel);
        lock_unpoison(&self.tokens).extend(tokens.iter().copied());
        match self.outcome {
            ScriptedPost::Accepted => BatchPostOutcome::AllAccepted,
            ScriptedPost::Unaccepted => BatchPostOutcome::PrefixAccepted {
                accepted: 0,
                first_unaccepted: 0,
                source: std::io::Error::from_raw_os_error(libc::ENOMEM),
            },
            ScriptedPost::Ambiguous => BatchPostOutcome::Ambiguous {
                source: std::io::Error::from_raw_os_error(libc::EIO),
            },
            ScriptedPost::DispatchDuringPost => {
                let token = tokens[0];
                let session = self.session.upgrade().expect("session owner");
                assert_eq!(
                    session.enqueue_completion(wc(token, self.qp_num, opcode)),
                    session.connections.lookup_qp(self.qp_num)
                );
                let connection = session
                    .connections
                    .lookup_qp(self.qp_num)
                    .expect("connection token");
                assert_eq!(
                    session.dispatch_connection_completions(connection, 1),
                    (1, false)
                );
                BatchPostOutcome::AllAccepted
            }
            ScriptedPost::PrefixAccepted(accepted) => BatchPostOutcome::PrefixAccepted {
                accepted,
                first_unaccepted: accepted,
                source: std::io::Error::from_raw_os_error(libc::ENOMEM),
            },
            ScriptedPost::PrefixWithQueuedSuffixCompletion {
                accepted,
                completed_suffix,
            } => {
                assert!(completed_suffix >= accepted);
                let token = tokens[completed_suffix];
                let session = self.session.upgrade().expect("session owner");
                assert_eq!(
                    session.enqueue_completion(wc(token, self.qp_num, opcode)),
                    session.connections.lookup_qp(self.qp_num)
                );
                BatchPostOutcome::PrefixAccepted {
                    accepted,
                    first_unaccepted: accepted,
                    source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                }
            }
            ScriptedPost::PrefixWithSuffixCompletion {
                accepted,
                completed_suffix,
            } => {
                assert!(completed_suffix >= accepted);
                let token = tokens[completed_suffix];
                let session = self.session.upgrade().expect("session owner");
                assert_eq!(
                    session.enqueue_completion(wc(token, self.qp_num, opcode)),
                    session.connections.lookup_qp(self.qp_num)
                );
                let connection = session
                    .connections
                    .lookup_qp(self.qp_num)
                    .expect("connection token");
                assert_eq!(
                    session.dispatch_connection_completions(connection, 1),
                    (1, false)
                );
                BatchPostOutcome::PrefixAccepted {
                    accepted,
                    first_unaccepted: accepted,
                    source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                }
            }
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn error_transitions(&self) -> usize {
        self.error_transitions.load(Ordering::Acquire)
    }

    fn tokens(&self) -> Vec<OperationToken> {
        lock_unpoison(&self.tokens).clone()
    }
}

impl WorkRequestPoster for ScriptedPoster {
    fn qp_num(&self) -> u32 {
        self.qp_num
    }

    fn capabilities(&self) -> Option<crate::v2::qp::QpCapabilities> {
        None
    }

    fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        Ok(self.post(
            (0..batch.len())
                .map(|index| OperationToken::decode(batch.wr_id_for_test(index)))
                .collect(),
            IBV_WC_SEND,
        ))
    }

    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        Ok(self.post(
            (0..batch.len())
                .map(|index| OperationToken::decode(batch.wr_id_for_test(index)))
                .collect(),
            IBV_WC_RECV,
        ))
    }

    fn to_error(
        &self,
        _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
    ) -> Result<()> {
        self.error_transitions.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn destroy_qp(
        &self,
        _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
    ) -> Result<bool> {
        Ok(false)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect(&self) -> Result<()> {
        Ok(())
    }
}

fn production_engine(
    connections: usize,
    operations: usize,
    cq_capacity: usize,
) -> Option<(
    super::super::RdmaEngine,
    super::super::RdmaEngineDriver,
    OperationOwners,
)> {
    let devices = crate::cm::RdmaCmDeviceList::new().ok()?;
    let device = devices
        .device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))?;
    drop(devices);
    let (engine, driver) = super::super::RdmaEngineBuilder::new(device)
        .completion_mode(super::super::CompletionMode::Polling)
        .maximum_live_connections(connections)
        .maximum_inflight_operations(operations)
        .cq_capacity(cq_capacity)
        .build()
        .expect("software-provider engine");
    let owners = OperationOwners {
        io_core: Arc::clone(&engine.shared.io_core),
        session: Arc::clone(&engine.shared.session),
        _runtime: engine.shared.clone(),
    };
    Some((engine, driver, owners))
}

fn scripted_connection(
    session: &Arc<SessionManager>,
    poster: Arc<ScriptedPoster>,
    send_wr: usize,
    recv_wr: usize,
) -> crate::v2::engine::session::connection::RdmaConnection {
    install_connection(
        session,
        poster,
        super::super::RdmaConnectionConfig::default()
            .max_send_wr(send_wr)
            .max_recv_wr(recv_wr),
        None,
        None,
    )
    .unwrap()
}

fn poll_once(operation: &mut RdmaOperation) -> Poll<(Result<Completion>, Option<Mr>)> {
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(operation).poll(&mut cx)
}

fn complete(
    session: &SessionManager,
    connection: &ConnectionState,
    token: OperationToken,
    opcode: u32,
) {
    assert_eq!(
        session.enqueue_completion(wc(token, connection.qp_num(), opcode)),
        Some(connection.token)
    );
    assert_eq!(
        session.dispatch_connection_completions(connection.token, 1),
        (1, false)
    );
}

struct NoopPoster(u32);

impl WorkRequestPoster for NoopPoster {
    fn qp_num(&self) -> u32 {
        self.0
    }
    fn capabilities(&self) -> Option<crate::v2::qp::QpCapabilities> {
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
        Ok(false)
    }
    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect(&self) -> Result<()> {
        Ok(())
    }
}

struct OperationOwners {
    io_core: Arc<IoCore>,
    session: Arc<SessionManager>,
    _runtime: Arc<dyn SessionEngineRuntime>,
}

fn synthetic_engine_root(capacity: usize) -> Arc<EngineShared> {
    let mut config = EngineConfig::new("test0".into());
    config.max_live_connections = capacity;
    config.max_inflight_operations = capacity;
    config.cq_capacity = capacity;
    EngineShared::new(config, None, None).unwrap().into_shared()
}

fn synthetic_engine(capacity: usize) -> OperationOwners {
    let engine = synthetic_engine_root(capacity);
    OperationOwners {
        io_core: Arc::clone(&engine.io_core),
        session: Arc::clone(&engine.session),
        _runtime: engine,
    }
}

fn synthetic_connection_on(
    owners: &OperationOwners,
    qp_num: u32,
) -> crate::v2::engine::session::connection::RdmaConnection {
    install_connection(
        &owners.session,
        Arc::new(NoopPoster(qp_num)),
        super::super::RdmaConnectionConfig::default()
            .max_send_wr(4)
            .max_recv_wr(4),
        None,
        None,
    )
    .unwrap()
}

fn synthetic_connection() -> Arc<ConnectionState> {
    synthetic_connection_on(&synthetic_engine(8), 7).into_state_without_close_for_test()
}

fn install_accepted(
    owners: &OperationOwners,
    connection: &Arc<ConnectionState>,
    opcode: WcOpcode,
) -> OperationToken {
    assert!(
        connection
            .reserve_local(match opcode {
                WcOpcode::Recv => Direction::Recv,
                _ => Direction::Send,
            })
            .is_ok()
    );
    assert!(owners.io_core.cq_credits.reserve());
    let (token, operation) = owners
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(connection),
                match opcode {
                    WcOpcode::Recv => Direction::Recv,
                    _ => Direction::Send,
                },
                opcode,
                None,
                1,
            ))
        })
        .unwrap();
    operation.commit_accepted();
    owners
        .io_core
        .accepted_operations
        .fetch_add(1, Ordering::AcqRel);
    token
}

fn install_accepted_with_result(
    owners: &OperationOwners,
    connection: &Arc<ConnectionState>,
    opcode: WcOpcode,
) -> (OperationToken, super::super::io::IoEventReceiver) {
    let direction = match opcode {
        WcOpcode::Recv => Direction::Recv,
        _ => Direction::Send,
    };
    connection.reserve_local(direction).unwrap();
    assert!(owners.io_core.cq_credits.reserve());
    let (sender, events) = super::super::io::event_port();
    let (token, operation) = owners
        .io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new_with_event(
                token,
                Arc::clone(connection),
                direction,
                opcode,
                None,
                1,
                Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
            ))
        })
        .unwrap();
    operation.commit_accepted();
    owners
        .io_core
        .accepted_operations
        .fetch_add(1, Ordering::AcqRel);
    (token, events)
}

fn wc(token: OperationToken, qp_num: u32, opcode: u32) -> WorkCompletion {
    let mut completion = WorkCompletion::default();
    completion.inner.wr_id = token.encode();
    completion.inner.qp_num = qp_num;
    completion.inner.opcode = opcode;
    completion.inner.status = IBV_WC_SUCCESS;
    completion
}
