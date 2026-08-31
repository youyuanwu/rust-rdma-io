//! Central engine lifecycle deadlines and terminal wedge calculation.

use std::sync::atomic::Ordering;

use super::{EngineFailure, EngineShared};

impl EngineShared {
    pub(super) fn shutdown_deadline_failure(&self) -> Option<EngineFailure> {
        if self.outcome().is_some() {
            return None;
        }
        let retained_bundles = self.retained_bundle_count();
        let outstanding_operations = self.unsafe_outstanding_operations();
        let pending_routes = self.cm.pending_route_count();
        if retained_bundles == 0 && outstanding_operations == 0 && pending_routes == 0 {
            return None;
        }
        self.diagnostic_counters
            .engine_wedges
            .fetch_add(1, Ordering::Relaxed);
        // Pending CM work can wedge shutdown without owning a retained bundle.
        Some(EngineFailure::Wedged {
            retained_bundles,
            outstanding_operations,
            cq_debt: outstanding_operations,
        })
    }

    pub(super) fn synchronously_prepare_driver_drop(&self) {
        for connection in self.connections.occupied() {
            let _lifecycle = connection.lock_lifecycle();
            connection.stop_posting();
            if let Ok(true) = connection.transition_to_error_once() {
                self.diagnostic_counters
                    .qp_error_transitions
                    .fetch_add(1, Ordering::Relaxed);
            }
            if connection.accepted_count() == 0
                && !connection.is_retired()
                && connection.poster.destroy_qp()
            {
                connection.mark_qp_destroyed();
                self.record_qp_destroy();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::super::config::{
        DEFAULT_CONNECTION_DRAIN_DEADLINE, DEFAULT_ENGINE_SHUTDOWN_DEADLINE,
        DEFAULT_MISSING_CQE_DEADLINE, EngineConfig,
    };
    use super::super::connection::{WorkRequestPoster, install_connection};
    use super::super::registry::OperationToken;
    use super::super::{CompletionMode, RdmaConnectionConfig, test_engine_pair};
    use crate::v2::error::{Error, Result};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct HeldPoster {
        qp_num: u32,
        destroys: AtomicUsize,
    }

    impl WorkRequestPoster for HeldPoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            unreachable!("lifecycle test does not post")
        }

        fn post_recv(&self, _: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            unreachable!("lifecycle test does not post")
        }

        fn to_error(&self) -> Result<()> {
            Ok(())
        }

        fn destroy_qp(&self) -> bool {
            self.destroys
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        }

        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        future.poll(&mut context)
    }

    #[test]
    fn lifecycle_deadline_defaults_and_bounds_are_exact() {
        let config = EngineConfig::new("test0".into());
        assert_eq!(config.missing_cqe_deadline, DEFAULT_MISSING_CQE_DEADLINE);
        assert_eq!(
            config.connection_drain_deadline,
            DEFAULT_CONNECTION_DRAIN_DEADLINE
        );
        assert_eq!(config.shutdown_deadline, DEFAULT_ENGINE_SHUTDOWN_DEADLINE);

        for deadline in [Duration::from_millis(1), Duration::from_secs(10 * 60)] {
            let mut valid = config.clone();
            valid.shutdown_deadline = deadline;
            valid.validate_without_provider().unwrap();
        }
        for deadline in [Duration::ZERO, Duration::from_secs(10 * 60 + 1)] {
            let mut invalid = config.clone();
            invalid.shutdown_deadline = deadline;
            assert!(invalid.validate_without_provider().is_err());
        }
    }

    #[test]
    fn driver_drop_counts_only_the_take_once_qp_destroy() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(HeldPoster {
            qp_num: 31,
            destroys: AtomicUsize::new(0),
        });
        install_connection(
            &engine.shared,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();

        engine.shared.synchronously_prepare_driver_drop();
        engine.shared.synchronously_prepare_driver_drop();

        assert_eq!(poster.destroys.load(Ordering::Acquire), 1);
        assert_eq!(engine.diagnostics().qp_destroys, 1);
        engine.shared.finish(super::super::EngineOutcome::Success);
        drop(driver);
    }

    #[tokio::test(start_paused = true)]
    async fn unresolved_shutdown_wedges_at_exact_thirty_second_deadline() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(HeldPoster {
            qp_num: 29,
            destroys: AtomicUsize::new(0),
        });
        let poster_dyn: Arc<dyn WorkRequestPoster> = poster.clone();
        let connection = install_connection(
            &engine.shared,
            poster_dyn,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let token = OperationToken {
            slot: 29,
            generation: 1,
        };
        connection.state.add_accepted(token);
        engine
            .shared
            .accepted_operations
            .fetch_add(1, Ordering::AcqRel);

        let mut shutdown = Box::pin(engine.shutdown());
        assert!(poll_once(shutdown.as_mut()).is_pending());
        assert!(poll_once(Pin::new(&mut driver)).is_pending());

        tokio::time::advance(Duration::from_millis(29_999)).await;
        assert!(poll_once(Pin::new(&mut driver)).is_pending());
        assert!(poll_once(shutdown.as_mut()).is_pending());
        assert_eq!(poster.destroys.load(Ordering::Acquire), 0);

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(matches!(
            poll_once(Pin::new(&mut driver)),
            Poll::Ready(Err(Error::EngineWedged {
                retained_bundles: 1,
                outstanding_operations: 1,
                cq_debt: 1
            }))
        ));
        assert!(matches!(
            poll_once(shutdown.as_mut()),
            Poll::Ready(Err(Error::EngineWedged {
                retained_bundles: 1,
                outstanding_operations: 1,
                cq_debt: 1
            }))
        ));
        let terminal = engine.diagnostics().terminal_error.unwrap();
        assert_eq!(terminal.class, "EngineWedged");
        assert_eq!(poster.destroys.load(Ordering::Acquire), 0);
    }
}
