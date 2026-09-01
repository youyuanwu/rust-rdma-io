//! Central engine lifecycle deadlines and terminal wedge calculation.

use std::sync::atomic::Ordering;

use super::{EngineShared, RdmaEngineTerminalError};
use crate::v2::error::{Error, Result};

pub(super) enum TakeOnceResult<T> {
    Pending,
    Ready(Result<T>),
    Taken,
}

/// Cloneable terminal outcome used by both engine-wide and object-local waiters.
///
/// Connection quarantine variants are connection-local close dispositions
/// only. They may be memoized by a connection, but must never become the
/// engine driver's terminal outcome; engine-wide terminal causes must match
/// the driver result.
#[derive(Clone)]
pub(super) struct MemoizedTerminalResult {
    result: Result<()>,
}

impl MemoizedTerminalResult {
    pub(super) fn success() -> Self {
        Self { result: Ok(()) }
    }

    pub(super) fn from_error(error: Error) -> Self {
        Self { result: Err(error) }
    }

    pub(super) fn is_success(&self) -> bool {
        self.result.is_ok()
    }

    pub(super) fn is_error(&self) -> bool {
        self.result.is_err()
    }

    pub(super) fn error(&self) -> Option<Error> {
        self.result.clone().err()
    }

    pub(super) fn is_connection_quarantined(&self) -> bool {
        matches!(
            self.result,
            Err(Error::ConnectionQuarantined { .. } | Error::ConnectionDestroyQuarantined { .. })
        )
    }

    pub(super) fn into_result(self) -> Result<()> {
        self.result
    }

    pub(super) fn summary(&self) -> Option<RdmaEngineTerminalError> {
        self.error().map(|error| RdmaEngineTerminalError {
            class: match error {
                Error::DriverShutdown => "DriverShutdown",
                Error::InvalidConfig(_) => "InvalidConfig",
                Error::EngineWedged { .. } => "EngineWedged",
                _ => "EngineError",
            }
            .into(),
            message: error.to_string(),
        })
    }
}

impl EngineShared {
    pub(super) fn shutdown_deadline_failure(&self) -> Option<Error> {
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
        Some(Error::EngineWedged {
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
                && !connection.retirement_is_quarantined()
            {
                match connection.establish_qp_destruction_boundary(&_lifecycle) {
                    Ok(true) => self.record_qp_destroy(),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(
                            qp_num = connection.qp_num(),
                            %error,
                            "failed to establish QP destruction boundary during driver drop"
                        );
                    }
                }
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
    use super::MemoizedTerminalResult;
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

        fn destroy_qp(&self) -> Result<bool> {
            Ok(self
                .destroys
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok())
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
        engine.shared.finish(MemoizedTerminalResult::success());
        drop(driver);
    }

    #[test]
    fn driver_drop_does_not_retry_destroy_quarantined_qp() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(HeldPoster {
            qp_num: 32,
            destroys: AtomicUsize::new(0),
        });
        let connection = install_connection(
            &engine.shared,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        connection.state.begin_close();
        assert!(connection.state.try_begin_retirement());
        connection.state.publish_destroy_quarantine(
            &Error::InvalidConfig("injected destroy failure".into()),
            || {},
        );

        engine.shared.synchronously_prepare_driver_drop();
        engine.shared.synchronously_prepare_driver_drop();

        assert_eq!(poster.destroys.load(Ordering::Acquire), 0);
        assert_eq!(engine.diagnostics().qp_destroys, 0);
        engine.shared.finish(MemoizedTerminalResult::success());
        drop(driver);
    }

    #[test]
    #[should_panic(expected = "ConnectionQuarantined is connection-local")]
    fn engine_terminal_rejects_connection_quarantined() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        engine.shared.finish(MemoizedTerminalResult::from_error(
            Error::ConnectionQuarantined {
                outstanding_operations: 1,
                cq_debt: 1,
            },
        ));
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
        assert_eq!(
            poster.destroys.load(Ordering::Acquire),
            1,
            "the fabricated unknown token is defensive-only; the live QP still has a safe boundary"
        );

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
        assert_eq!(poster.destroys.load(Ordering::Acquire), 1);
    }
}
