//! Exact accepted-WR connection drain and quarantine lifecycle.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::super::EngineShared;
use super::super::lifecycle::MemoizedTerminalResult;
use super::super::registry::{ConnectionToken, Lookup, read_unpoison};
use super::super::scheduler::DeadlineKind;
use super::SessionManager;
use super::connection::ConnectionState;

impl SessionManager {
    pub(crate) fn begin_connection_close(
        &self,
        shared: &EngineShared,
        connection: &Arc<ConnectionState>,
    ) {
        if connection.is_retired() {
            return;
        }
        let admission = read_unpoison(&self.admission);
        let lifecycle = connection.lock_lifecycle();
        let first = connection.begin_close();
        let mut close_effects = None;
        if first {
            match self.transition_connection_to_error(connection) {
                Ok(_) => {}
                Err(error) => {
                    let event = connection.mark_cm_failure(error.clone());
                    connection.rollback_draining_count();
                    drop(lifecycle);
                    drop(admission);
                    if let Some(event) = event {
                        event.deliver();
                    }
                    shared.finish(MemoizedTerminalResult::from_error(error));
                    return;
                }
            }
            shared
                .work_signal
                .publish(super::super::driver::CQ_RECHECK_WORK);

            let engine_is_terminating = shared.shutdown_requested.load(Ordering::Acquire);
            if !engine_is_terminating {
                let error = connection.operation_close_error();
                let report = connection.io_drain_report();
                close_effects = Some(
                    self.io_core
                        .fail_observers_for_close(&report.accepted_tokens, error),
                );
            }
        }
        drop(lifecycle);
        drop(admission);
        if let Some(effects) = close_effects {
            effects.publish();
        }
        if first {
            self.schedule_connection_drain(shared, connection.token);
        }
        if connection.accepted_count() == 0 {
            self.record_connection_drained(connection);
            self.schedule_connection_retirement(shared, connection);
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn begin_all_connection_close(&self, shared: &EngineShared) {
        if self
            .shutdown_connection_close_started
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        for connection in self.connections.occupied() {
            self.begin_connection_close(shared, &connection);
        }
    }

    pub(in crate::v2::engine) fn schedule_connection_retirement(
        &self,
        shared: &EngineShared,
        connection: &ConnectionState,
    ) {
        if connection.is_retired()
            || connection.retirement_is_quarantined()
            || (connection.close_started() && !connection.error_transition_complete())
            || !connection.try_request_retirement()
        {
            return;
        }
        self.cm.enqueue_retirement(connection.token);
        shared.work_signal.publish(super::cm::CM_WORK);
    }

    fn schedule_connection_drain(&self, shared: &EngineShared, token: ConnectionToken) {
        self.schedule_deadline(
            &shared.work_signal,
            DeadlineKind::ConnectionDrain,
            token.encode(),
            shared.config.connection_drain_deadline,
        );
    }

    pub(in crate::v2::engine) fn handle_connection_drain_deadline(
        &self,
        shared: &EngineShared,
        token: ConnectionToken,
    ) {
        let Lookup::Occupied(connection) = self.connections.lookup(token) else {
            return;
        };
        // CQEs already copied out of the hardware CQ must take the ordinary
        // quantum-bounded ready path before a destructive fallback can run.
        if connection.has_copied_completions() {
            self.io_core.publish_connection(&connection.io);
            self.schedule_deadline(
                &shared.work_signal,
                DeadlineKind::ConnectionDrain,
                token.encode(),
                std::time::Duration::ZERO,
            );
            return;
        }
        let forced_tokens = {
            let _admission = read_unpoison(&self.admission);
            let lifecycle = connection.lock_lifecycle();
            let tokens = connection.io_drain_report().accepted_tokens;
            if tokens.is_empty() {
                None
            } else {
                match self.establish_qp_destruction_proof(&connection, &lifecycle) {
                    Ok(proof) => Some((tokens, proof)),
                    Err(error) => {
                        tracing::warn!(
                            qp_num = connection.qp_num(),
                            %error,
                            "failed to establish result-aware QP destruction boundary"
                        );
                        None
                    }
                }
            }
        };
        if let Some((tokens, proof)) = forced_tokens {
            self.reclaim_after_qp_destroy(shared, proof, &connection, tokens);
            if self.reject_queued_completions_after_qp_destroy(shared, &connection) {
                self.io_core.publish_connection(&connection.io);
                self.schedule_deadline(
                    &shared.work_signal,
                    DeadlineKind::ConnectionDrain,
                    token.encode(),
                    std::time::Duration::ZERO,
                );
                return;
            }
        }
        if let Some(report) = connection.begin_quarantine() {
            self.track_connection_quarantine(connection.token);
            for operation in connection.accepted_tokens() {
                self.quarantine_operation(shared, operation);
            }
            if let Some(event) =
                connection.publish_quarantine(report.outstanding_operations, report.cq_debt)
            {
                event.deliver();
            }
        }
        if connection.close_started() && connection.accepted_count() == 0 {
            self.recover_connection_quarantine(&connection);
            self.record_connection_drained(&connection);
            self.schedule_connection_retirement(shared, &connection);
        }
    }

    pub(in crate::v2::engine) fn recover_connection_quarantine(
        &self,
        connection: &ConnectionState,
    ) {
        if !connection.recover_quarantine() {
            return;
        }
        self.recover_connection_quarantine_entry(connection.token);
    }

    pub(in crate::v2::engine) fn record_connection_drained(&self, connection: &ConnectionState) {
        connection.mark_drained_once();
    }

    pub(in crate::v2::engine) fn record_connection_retired(&self, connection: &ConnectionState) {
        // Successful retirement may clear the connection-level marker after
        // exact accepted-WR accounting reached zero. Any operation-level
        // quarantine entry remains tracked, so a mismatched retirement cannot
        // make retained debt appear recovered.
        let _ = self.clear_connection_quarantine(connection.token);
    }
}

#[cfg(test)]
impl EngineShared {
    pub(in crate::v2::engine) fn begin_all_connection_close(&self) {
        self.session.begin_all_connection_close(self);
    }

    pub(in crate::v2::engine) fn schedule_connection_retirement(
        &self,
        connection: &ConnectionState,
    ) {
        self.session
            .schedule_connection_retirement(self, connection);
    }

    pub(in crate::v2::engine) fn handle_connection_drain_deadline(&self, token: ConnectionToken) {
        self.session.handle_connection_drain_deadline(self, token);
    }

    pub(in crate::v2::engine) fn recover_connection_quarantine(
        &self,
        connection: &ConnectionState,
    ) {
        self.session.recover_connection_quarantine(connection);
    }

    pub(in crate::v2::engine) fn record_connection_drained(&self, connection: &ConnectionState) {
        self.session.record_connection_drained(connection);
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

    use super::super::super::io_core::install_accepted_operation_for_driver_test;
    use super::super::super::registry::OperationToken;
    use super::super::super::{CompletionMode, RdmaConnectionConfig, test_engine_pair};
    use super::super::connection::{WorkRequestPoster, install_connection};
    use crate::v2::error::{Error, Result};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct TestPoster {
        qp_num: u32,
        error_transitions: AtomicUsize,
        destroys: AtomicUsize,
        fail_error_transition: bool,
        fail_destroy: bool,
    }

    impl TestPoster {
        fn new(qp_num: u32) -> Arc<Self> {
            Arc::new(Self {
                qp_num,
                error_transitions: AtomicUsize::new(0),
                destroys: AtomicUsize::new(0),
                fail_error_transition: false,
                fail_destroy: false,
            })
        }

        fn failing(qp_num: u32) -> Arc<Self> {
            Arc::new(Self {
                qp_num,
                error_transitions: AtomicUsize::new(0),
                destroys: AtomicUsize::new(0),
                fail_error_transition: true,
                fail_destroy: false,
            })
        }

        fn destroy_failing(qp_num: u32) -> Arc<Self> {
            Arc::new(Self {
                qp_num,
                error_transitions: AtomicUsize::new(0),
                destroys: AtomicUsize::new(0),
                fail_error_transition: false,
                fail_destroy: true,
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

        fn destroy_qp(&self) -> Result<bool> {
            self.destroys.fetch_add(1, Ordering::AcqRel);
            if self.fail_destroy {
                Err(Error::Verbs(std::io::Error::from_raw_os_error(libc::EBUSY)))
            } else {
                Ok(true)
            }
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
        assert_eq!(engine.shared.connection_admission.snapshot().draining, 0);
        drop(driver);
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        future.poll(&mut context)
    }

    fn install_accepted_connection(
        engine: &super::super::super::RdmaEngine,
        qp_num: u32,
    ) -> (
        super::super::super::RdmaConnection,
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
    async fn ambiguous_operation_ownership_quarantines_until_exact_removal() {
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
        let published = engine.diagnostics();
        assert_eq!(published.quarantined_connections, 1);
        assert_eq!(
            engine
                .shared
                .connection_admission
                .snapshot()
                .registered_live_qps,
            0
        );
        assert_eq!(
            poster.destroys.load(Ordering::Acquire),
            1,
            "the defensive unknown-token branch retains accounting after a safe QP boundary"
        );

        assert!(connection.state.remove_accepted(token));
        engine
            .shared
            .accepted_operations
            .fetch_sub(1, Ordering::AcqRel);
        engine
            .shared
            .recover_connection_quarantine(&connection.state);
        assert_eq!(
            engine
                .shared
                .connection_admission
                .snapshot()
                .registered_live_qps,
            0
        );
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

    #[tokio::test(start_paused = true)]
    async fn registered_operation_is_retained_when_qp_destruction_fails() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = TestPoster::destroy_failing(23);
        let connection = install_connection(
            &engine.shared,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        install_accepted_operation_for_driver_test(
            &engine.shared,
            &connection.state,
            crate::wc::WcOpcode::Recv,
        );
        let mut close = Box::pin(connection.close());

        assert!(poll_once(close.as_mut()).is_pending());
        assert!(poll_once(Pin::new(&mut driver)).is_pending());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(poll_once(Pin::new(&mut driver)).is_pending());
        assert!(matches!(
            poll_once(close.as_mut()),
            Poll::Ready(Err(Error::ConnectionQuarantined {
                outstanding_operations: 1,
                cq_debt: 1
            }))
        ));

        let diagnostics = engine.diagnostics();
        assert_eq!(poster.destroys.load(Ordering::Acquire), 1);
        assert_eq!(diagnostics.registered_operations, 1);
        assert_eq!(diagnostics.accepted_operations, 1);
        assert_eq!(diagnostics.available_cq_credits, 16_383);
        assert_eq!(diagnostics.retained_cq_credits, 1);
        assert_eq!(diagnostics.quarantined_operations, 1);
        assert_eq!(diagnostics.quarantined_connections, 1);

        drop(driver);
    }

    #[tokio::test(start_paused = true)]
    async fn anomalous_token_does_not_strand_reclaimable_operations_after_qp_destroy() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let poster = TestPoster::new(24);
        let connection = install_connection(
            &engine.shared,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        for opcode in [crate::wc::WcOpcode::Send, crate::wc::WcOpcode::Recv] {
            install_accepted_operation_for_driver_test(&engine.shared, &connection.state, opcode);
        }
        let anomalous = OperationToken {
            slot: u32::MAX,
            generation: 1,
        };
        connection.state.add_accepted(anomalous);
        engine
            .shared
            .accepted_operations
            .fetch_add(1, Ordering::AcqRel);
        connection.state.begin_close();
        engine
            .shared
            .session
            .transition_connection_to_error(&connection.state)
            .unwrap();

        engine
            .shared
            .handle_connection_drain_deadline(connection.state.token);

        let diagnostics = engine.diagnostics();
        assert_eq!(poster.destroys.load(Ordering::Acquire), 1);
        assert_eq!(diagnostics.registered_operations, 0);
        assert_eq!(diagnostics.accepted_operations, 1);
        assert_eq!(connection.state.accepted_tokens(), vec![anomalous]);
        assert_eq!(diagnostics.quarantined_connections, 1);
    }
}
