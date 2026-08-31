//! Exact accepted-WR connection drain and quarantine lifecycle.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::connection::ConnectionState;
use super::registry::{ConnectionToken, Lookup, read_unpoison};
use super::scheduler::DeadlineKind;
use super::{EngineFailure, EngineOutcome, EngineShared};

impl EngineShared {
    pub(crate) fn begin_connection_close(&self, connection: &Arc<ConnectionState>) {
        if connection.is_retired() {
            return;
        }
        let admission = read_unpoison(&self.admission);
        let lifecycle = connection.lock_lifecycle();
        let first = connection.begin_close();
        let mut operations_to_wake = Vec::new();
        if first {
            self.diagnostic_counters
                .connections_drain_started
                .fetch_add(1, Ordering::Relaxed);

            match connection.transition_to_error_once() {
                Ok(true) => {
                    self.diagnostic_counters
                        .qp_error_transitions
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(false) => {}
                Err(error) => {
                    connection.mark_cm_failure(error.clone());
                    connection.rollback_draining_count();
                    drop(lifecycle);
                    drop(admission);
                    self.finish(EngineOutcome::Failure(EngineFailure::from_progress(error)));
                    return;
                }
            }

            let engine_is_terminating = self.shutdown_requested.load(Ordering::Acquire);
            if !engine_is_terminating {
                let error = connection.operation_close_error();
                for token in connection.accepted_tokens() {
                    if let Lookup::Occupied(operation) = self.operations.lookup(token)
                        && operation.fail_observer_for_close(error.clone())
                    {
                        operations_to_wake.push(operation);
                    }
                }
            }
        }
        drop(lifecycle);
        drop(admission);
        for operation in operations_to_wake {
            operation.wake();
        }
        if first {
            self.schedule_connection_drain(connection.token);
        }
        if connection.accepted_count() == 0 {
            self.record_connection_drained(connection);
            self.schedule_connection_retirement(connection);
        }
    }

    pub(super) fn begin_all_connection_close(&self) {
        if self
            .shutdown_connection_close_started
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        for connection in self.connections.occupied() {
            self.begin_connection_close(&connection);
        }
    }

    pub(super) fn schedule_connection_retirement(&self, connection: &ConnectionState) {
        if connection.is_retired()
            || (connection.close_started() && !connection.error_transition_complete())
            || !connection.try_request_retirement()
        {
            return;
        }
        self.cm.enqueue_retirement(connection.token);
        self.work_signal.publish(super::cm::CM_WORK);
    }

    fn schedule_connection_drain(&self, token: ConnectionToken) {
        self.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            token.encode(),
            self.config.connection_drain_deadline,
        );
    }

    pub(super) fn handle_connection_drain_deadline(&self, token: ConnectionToken) {
        let Lookup::Occupied(connection) = self.connections.lookup(token) else {
            return;
        };
        if let Some((_outstanding, _cq_debt)) = connection.apply_drain_deadline() {
            if self.track_connection_quarantine(connection.token) {
                self.diagnostic_counters
                    .connections_quarantined
                    .fetch_add(1, Ordering::Relaxed);
            }
            self.diagnostic_counters
                .connection_quarantine_outcomes
                .fetch_add(1, Ordering::Relaxed);
            for operation in connection.accepted_tokens() {
                self.quarantine_operation(operation);
            }
        }
        if connection.close_started() && connection.accepted_count() == 0 {
            self.recover_connection_quarantine(&connection);
            self.record_connection_drained(&connection);
            self.schedule_connection_retirement(&connection);
        }
    }

    pub(super) fn recover_connection_quarantine(&self, connection: &ConnectionState) {
        if !connection.recover_quarantine() {
            return;
        }
        self.recover_connection_quarantine_entry(connection.token);
    }

    pub(super) fn record_connection_drained(&self, connection: &ConnectionState) {
        if connection.mark_drained_once() {
            self.diagnostic_counters
                .connections_drained
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn record_qp_destroy(&self) {
        self.diagnostic_counters
            .qp_destroys
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_connection_retired(&self, connection: &ConnectionState) {
        // Successful retirement may clear the connection-level marker after
        // exact accepted-WR accounting reached zero. Any operation-level
        // quarantine entry remains tracked, so a mismatched retirement cannot
        // make retained debt appear recovered.
        let _ = self.clear_connection_quarantine(connection.token);
        self.diagnostic_counters
            .connections_closed
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_connection_retirement_failure(&self, _connection: &ConnectionState) {
        // Fail closed: keep the quarantine entry and oldest-age gauge pinned.
        // A failed generation retirement cannot prove that routing identity or
        // retained provider ownership is safe to recycle.
        self.diagnostic_counters
            .connections_failed
            .fetch_add(1, Ordering::Relaxed);
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

    use super::super::connection::{WorkRequestPoster, install_connection};
    use super::super::registry::OperationToken;
    use super::super::{CompletionMode, RdmaConnectionConfig, test_engine_pair};
    use crate::v2::error::{Error, Result};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct TestPoster {
        qp_num: u32,
        error_transitions: AtomicUsize,
        destroys: AtomicUsize,
        fail_error_transition: bool,
    }

    impl TestPoster {
        fn new(qp_num: u32) -> Arc<Self> {
            Arc::new(Self {
                qp_num,
                error_transitions: AtomicUsize::new(0),
                destroys: AtomicUsize::new(0),
                fail_error_transition: false,
            })
        }

        fn failing(qp_num: u32) -> Arc<Self> {
            Arc::new(Self {
                qp_num,
                error_transitions: AtomicUsize::new(0),
                destroys: AtomicUsize::new(0),
                fail_error_transition: true,
            })
        }
    }

    impl WorkRequestPoster for TestPoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            unreachable!("drain test does not post")
        }

        fn post_recv(&self, _: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            unreachable!("drain test does not post")
        }

        fn to_error(&self) -> Result<()> {
            self.error_transitions.fetch_add(1, Ordering::AcqRel);
            if self.fail_error_transition {
                Err(Error::Verbs(std::io::Error::other(
                    "injected QP ERR transition failure",
                )))
            } else {
                Ok(())
            }
        }

        fn destroy_qp(&self) -> bool {
            self.destroys.fetch_add(1, Ordering::AcqRel);
            true
        }

        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_error_transition_rolls_back_the_draining_gauge() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let poster = TestPoster::failing(19);
        let connection = install_connection(
            &engine.shared,
            poster,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let mut close = Box::pin(connection.close());
        assert!(matches!(
            poll_once(close.as_mut()),
            Poll::Ready(Err(Error::Verbs(_)))
        ));
        assert_eq!(engine.diagnostics().draining_connection_reservations, 0);
        drop(driver);
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        future.poll(&mut context)
    }

    fn install_accepted_connection(
        engine: &super::super::RdmaEngine,
        qp_num: u32,
    ) -> (
        super::super::RdmaConnection,
        Arc<TestPoster>,
        OperationToken,
    ) {
        let poster = TestPoster::new(qp_num);
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
            slot: qp_num,
            generation: 1,
        };
        connection.state.add_accepted(token);
        engine
            .shared
            .accepted_operations
            .fetch_add(1, Ordering::AcqRel);
        (connection, poster, token)
    }

    #[tokio::test(start_paused = true)]
    async fn close_deadline_is_exact_memoized_and_recovers_only_from_exact_removal() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let (connection, poster, token) = install_accepted_connection(&engine, 17);
        let mut close = Box::pin(connection.close());

        assert!(poll_once(close.as_mut()).is_pending());
        assert!(poll_once(Pin::new(&mut driver)).is_pending());
        assert_eq!(poster.error_transitions.load(Ordering::Acquire), 1);

        tokio::time::advance(Duration::from_millis(4_999)).await;
        assert!(poll_once(Pin::new(&mut driver)).is_pending());
        assert!(poll_once(close.as_mut()).is_pending());

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(poll_once(Pin::new(&mut driver)).is_pending());
        assert!(matches!(
            poll_once(close.as_mut()),
            Poll::Ready(Err(Error::ConnectionQuarantined {
                outstanding_operations: 1,
                cq_debt: 1
            }))
        ));
        assert_eq!(engine.diagnostics().quarantined_bundles, 1);
        assert_eq!(poster.destroys.load(Ordering::Acquire), 0);

        assert!(connection.state.remove_accepted(token));
        engine
            .shared
            .accepted_operations
            .fetch_sub(1, Ordering::AcqRel);
        engine
            .shared
            .recover_connection_quarantine(&connection.state);
        engine.shared.record_connection_drained(&connection.state);
        engine
            .shared
            .schedule_connection_retirement(&connection.state);
        engine
            .shared
            .cm
            .service_software(&engine.shared, None, 32)
            .unwrap();
        assert_eq!(poster.destroys.load(Ordering::Acquire), 1);

        let mut repeated = Box::pin(connection.close());
        assert!(matches!(
            poll_once(repeated.as_mut()),
            Poll::Ready(Err(Error::ConnectionQuarantined {
                outstanding_operations: 1,
                cq_debt: 1
            }))
        ));

        engine.shared.request_shutdown();
        let mut completed = false;
        for _ in 0..4 {
            if matches!(poll_once(Pin::new(&mut driver)), Poll::Ready(Ok(()))) {
                completed = true;
                break;
            }
        }
        assert!(completed);
    }
}
