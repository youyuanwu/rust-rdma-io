//! Central engine lifecycle deadlines and terminal wedge calculation.

use super::session::SessionReactorSources;
use super::{RdmaEngineLifecycle, RdmaEngineTerminalError};
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

pub(super) struct EngineLifecycleState {
    lifecycle: RdmaEngineLifecycle,
    shutdown_requested: bool,
    pending_terminal: Option<MemoizedTerminalResult>,
    terminal: Option<MemoizedTerminalResult>,
}

impl EngineLifecycleState {
    pub(super) fn new() -> Self {
        Self {
            lifecycle: RdmaEngineLifecycle::Created,
            shutdown_requested: false,
            pending_terminal: None,
            terminal: None,
        }
    }

    pub(super) fn lifecycle(&self) -> RdmaEngineLifecycle {
        self.lifecycle
    }

    pub(super) fn transition_running(&mut self) {
        if self.lifecycle == RdmaEngineLifecycle::Created {
            self.lifecycle = RdmaEngineLifecycle::Running;
        }
    }

    pub(super) fn request_shutdown(&mut self) -> bool {
        if self.shutdown_requested {
            return false;
        }
        self.shutdown_requested = true;
        if !matches!(
            self.lifecycle,
            RdmaEngineLifecycle::Terminated | RdmaEngineLifecycle::Failed
        ) {
            self.lifecycle = RdmaEngineLifecycle::ShutdownRequested;
        }
        true
    }

    pub(super) fn begin_failure(&mut self, error: Error) -> bool {
        if self.pending_terminal.is_some() || self.terminal.is_some() {
            return false;
        }
        self.pending_terminal = Some(MemoizedTerminalResult::from_error(error));
        self.request_shutdown();
        true
    }

    pub(super) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub(super) fn pending_terminal(&self) -> Option<MemoizedTerminalResult> {
        self.pending_terminal.clone()
    }

    pub(super) fn is_terminalizing_failure(&self) -> bool {
        self.pending_terminal.is_some()
    }

    pub(super) fn shutdown_is_pending(&self) -> bool {
        self.shutdown_requested && self.terminal.is_none()
    }

    pub(super) fn outcome(&self) -> Option<MemoizedTerminalResult> {
        self.terminal.clone()
    }

    pub(super) fn finish(&mut self, outcome: MemoizedTerminalResult) -> bool {
        if self.terminal.is_some() {
            return false;
        }
        self.shutdown_requested = true;
        self.lifecycle = if outcome.is_success() {
            RdmaEngineLifecycle::Terminated
        } else {
            RdmaEngineLifecycle::Failed
        };
        self.terminal = Some(outcome);
        true
    }
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

impl SessionReactorSources {
    pub(super) fn synchronously_prepare_driver_drop(
        &mut self,
        _io_core: &mut super::io_core::IoState,
    ) {
        for token in self.connections.occupied() {
            self.connections
                .with_connection_mut(token, |connection| connection.stop_posting());
            if self.connections.is_quarantined(token) {
                continue;
            }
            let _ = self
                .manager
                .transition_connection_to_error(&mut self.connections, token);
            if self.connections.accepted_count(token) == 0
                && !self.connections.retirement_is_quarantined(token)
            {
                match self
                    .manager
                    .ensure_qp_destroyed(&mut self.connections, token)
                {
                    Ok(()) => {}
                    Err(error) => {
                        let qp_num = self
                            .connections
                            .with_connection(token, |connection| connection.qp_num())
                            .unwrap_or(0);
                        tracing::warn!(
                            qp_num,
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
    use super::super::session::connection::{TestConnectionProvider, install_connection};
    use super::super::{CompletionMode, RdmaConnectionConfig, test_engine_pair};
    use super::MemoizedTerminalResult;
    use crate::v2::error::{Error, Result};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct HeldPoster {
        qp_num: u32,
        to_errors: AtomicUsize,
        destroys: AtomicUsize,
    }

    impl TestConnectionProvider for HeldPoster {
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
            self.to_errors.fetch_add(1, Ordering::AcqRel);
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
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(HeldPoster {
            qp_num: 31,
            to_errors: AtomicUsize::new(0),
            destroys: AtomicUsize::new(0),
        });
        install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster) as Arc<dyn TestConnectionProvider>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();

        driver
            .reactor
            .session
            .synchronously_prepare_driver_drop(driver.reactor.io.core_mut());
        driver
            .reactor
            .session
            .synchronously_prepare_driver_drop(driver.reactor.io.core_mut());

        assert_eq!(poster.destroys.load(Ordering::Acquire), 1);
        driver
            .reactor
            .finish_for_test(&engine.shared, MemoizedTerminalResult::success());
        drop(driver);
    }

    #[test]
    fn driver_drop_does_not_retry_destroy_quarantined_qp() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(HeldPoster {
            qp_num: 32,
            to_errors: AtomicUsize::new(0),
            destroys: AtomicUsize::new(0),
        });
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster) as Arc<dyn TestConnectionProvider>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let token = connection.session_token();
        assert!(driver.reactor.session.connections.begin_close(token));
        driver
            .reactor
            .session
            .manager
            .transition_connection_to_error(&mut driver.reactor.session.connections, token)
            .unwrap();
        assert!(driver.reactor.session.connections.request_retirement(token));
        assert!(driver.reactor.session.connections.begin_retirement(token));
        driver
            .reactor
            .session
            .connections
            .track_bundle_quarantine(token);
        let (_, event) = driver
            .reactor
            .session
            .connections
            .with_connection_mut(token, |connection| {
                connection.publish_destroy_quarantine(
                    &Error::InvalidConfig("injected destroy failure".into()),
                    || {},
                )
            })
            .unwrap();
        if let Some(event) = event {
            event.deliver();
        }

        driver
            .reactor
            .session
            .synchronously_prepare_driver_drop(driver.reactor.io.core_mut());
        driver
            .reactor
            .session
            .synchronously_prepare_driver_drop(driver.reactor.io.core_mut());

        assert_eq!(poster.destroys.load(Ordering::Acquire), 0);
        driver
            .reactor
            .finish_for_test(&engine.shared, MemoizedTerminalResult::success());
        drop(driver);
    }

    #[test]
    fn driver_drop_preserves_nonretiring_quarantined_qp() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(HeldPoster {
            qp_num: 33,
            to_errors: AtomicUsize::new(0),
            destroys: AtomicUsize::new(0),
        });
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster) as Arc<dyn TestConnectionProvider>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let token = connection.session_token();
        assert!(
            driver
                .reactor
                .session
                .connections
                .track_bundle_quarantine(token)
        );

        driver
            .reactor
            .terminate_on_driver_drop(&engine.shared)
            .publish();
        assert_eq!(poster.to_errors.load(Ordering::Acquire), 0);
        assert_eq!(poster.destroys.load(Ordering::Acquire), 0);
        drop(driver);
    }

    #[test]
    #[should_panic(expected = "ConnectionQuarantined is connection-local")]
    fn engine_terminal_rejects_connection_quarantined() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        driver.reactor.finish_for_test(
            &engine.shared,
            MemoizedTerminalResult::from_error(Error::ConnectionQuarantined {
                outstanding_operations: 1,
                cq_debt: 1,
            }),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unresolved_shutdown_wedges_at_exact_thirty_second_deadline() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(HeldPoster {
            qp_num: 29,
            to_errors: AtomicUsize::new(0),
            destroys: AtomicUsize::new(0),
        });
        let poster_dyn: Arc<dyn TestConnectionProvider> = poster.clone();
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            poster_dyn,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let unresolved = crate::v2::engine::registry::OperationToken {
            slot: 29,
            generation: 1,
        };
        let connection_token = connection.session_token();
        let (io, session) = (&mut driver.reactor.io, &mut driver.reactor.session);
        session
            .connections
            .with_connection_io_mut(connection_token, |connection, connection_io, _poster| {
                io.core_mut()
                    .add_accepted(connection, connection_io, unresolved);
            })
            .unwrap();
        driver.reactor.io.core_mut().accepted_operations += 1;

        let mut shutdown = Box::pin(engine.shutdown());
        assert!(poll_once(shutdown.as_mut()).is_pending());
        assert!(poll_once(Pin::new(&mut driver)).is_pending());

        tokio::time::advance(Duration::from_millis(29_999)).await;
        for _ in 0..4 {
            assert!(poll_once(Pin::new(&mut driver)).is_pending());
            if poster.destroys.load(Ordering::Acquire) == 1 {
                break;
            }
        }
        assert!(poll_once(shutdown.as_mut()).is_pending());
        assert_eq!(
            poster.destroys.load(Ordering::Acquire),
            1,
            "the fabricated unknown token is defensive-only; the live QP still has a safe boundary"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        let mut driver_result = Poll::Pending;
        for _ in 0..8 {
            driver_result = poll_once(Pin::new(&mut driver));
            if driver_result.is_ready() {
                break;
            }
        }
        assert!(matches!(
            driver_result,
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
