//! Exact accepted-WR connection drain and quarantine lifecycle.

#[cfg(test)]
use std::sync::atomic::Ordering;

use super::super::registry::{ConnectionToken, Lookup, read_unpoison};
use super::DeadlineKind;
use super::SessionManager;
use super::cm::CmState;
use super::registry::ConnectionRegistry;
use crate::v2::error::Error;

impl SessionManager {
    fn scan_close_observers_into(
        &self,
        io_core: &mut super::super::io_core::IoState,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
        reserve: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> bool {
        let Some((slot, complete)) = connections.close_scan(token) else {
            return true;
        };
        if complete {
            return true;
        }
        let scan_budget = actions.remaining().saturating_sub(reserve).min(32);
        if scan_budget == 0 {
            return false;
        }
        let error = connections
            .with_connection(token, |connection| connection.operation_close_error())
            .unwrap_or(Error::TransportClosed);
        let (effects, next, complete) =
            io_core.scan_connection_observers_for_close(token, slot, error, scan_budget);
        connections.update_close_scan(token, next, complete);
        effects.append_to(actions);
        complete
    }

    fn scan_quarantine_operations_into(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut super::super::io_core::IoState,
        token: ConnectionToken,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> bool {
        let Some((slot, complete)) = connections.quarantine_scan(token) else {
            return true;
        };
        if complete {
            return true;
        }
        let Some((effects, next, complete)) =
            connections.with_connection_io_mut(token, |_io, connection_io, _poster| {
                io_core.scan_connection_quarantine(token, connection_io, slot, 32)
            })
        else {
            return true;
        };
        connections.update_quarantine_scan(token, next, complete);
        self.commit_io_effects_into(connections, effects, actions);
        complete
    }

    #[cfg(test)]
    pub(crate) fn begin_connection_close(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut super::super::io_core::IoState,
        token: ConnectionToken,
    ) {
        if !matches!(connections.lookup(token), Lookup::Occupied(_)) {
            return;
        }

        let admission = read_unpoison(&self.frontend.admission);
        let first = connections.begin_close(token);
        let mut close_effects = None;
        let mut publish_io_work = false;
        if first {
            match self.transition_connection_to_error(connections, token) {
                Ok(_) => {}
                Err(error) => {
                    let event = connections
                        .with_connection_mut(token, |connection| {
                            connection.record_cm_failure(error.clone())
                        })
                        .flatten();
                    connections.fail_close_into_quarantine(token);
                    drop(admission);
                    if let Some(event) = event {
                        event.deliver();
                    }
                    connections.wake_close(token);
                    self.begin_driver_failure(error);
                    return;
                }
            }

            publish_io_work = true;

            let engine_is_terminating = self.shutdown_requested();
            if !engine_is_terminating {
                let error = connections
                    .with_connection(token, |connection| connection.operation_close_error())
                    .unwrap_or(Error::TransportClosed);
                let tokens = connections.accepted_tokens_bounded(token, usize::MAX);
                close_effects = Some(io_core.fail_observers_for_close(&tokens, error));
            }
        }
        drop(admission);
        if publish_io_work {
            self.publish_io_work();
        }
        if let Some(effects) = close_effects {
            effects.publish();
        }
        if first {
            self.schedule_connection_drain(connections, token);
        }
        if connections.accepted_count(token) == 0 {
            self.record_connection_drained(connections, token);
            self.schedule_connection_retirement(connections, token);
        }
    }

    pub(crate) fn begin_connection_close_into(
        &self,
        cm: &mut CmState,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
        io_core: &mut super::super::io_core::IoState,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        if !matches!(connections.lookup(token), Lookup::Occupied(_)) {
            return;
        }
        match cm.prepare_connection_close(connections, token, actions) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                self.begin_driver_failure(error);
                return;
            }
        }
        let admission = read_unpoison(&self.frontend.admission);
        let first = connections.begin_close(token);
        let mut close_publication_remaining = false;
        let mut publish_io_work = false;
        if first {
            match self.transition_connection_to_error(connections, token) {
                Ok(_) => {}
                Err(error) => {
                    let event = connections
                        .with_connection_mut(token, |connection| {
                            connection.record_cm_failure(error.clone())
                        })
                        .flatten();
                    connections.fail_close_into_quarantine(token);
                    drop(admission);
                    if let Some(event) = event {
                        actions.push_event(event);
                    }
                    connections.wake_close_into(token, actions);
                    self.begin_driver_failure(error);
                    return;
                }
            }
            publish_io_work = true;
            if !self.shutdown_requested() {
                close_publication_remaining =
                    !self.scan_close_observers_into(io_core, connections, token, 2, actions);
            }
        }
        drop(admission);
        if publish_io_work {
            self.publish_io_work();
        }
        if first {
            if close_publication_remaining {
                self.schedule_connection_drain_now(connections, token);
            } else {
                self.schedule_connection_drain(connections, token);
            }
        }
        if connections.accepted_count(token) == 0 {
            self.record_connection_drained(connections, token);
            self.schedule_connection_retirement(connections, token);
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn begin_all_connection_close(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut super::super::io_core::IoState,
    ) {
        if self
            .shutdown_connection_close_started
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        for token in connections.occupied() {
            self.begin_connection_close(connections, io_core, token);
        }
    }

    pub(in crate::v2::engine) fn schedule_connection_retirement(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) {
        if !connections.request_retirement(token) {
            return;
        }
        self.publish_session_work();
    }

    fn schedule_connection_drain(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) {
        connections.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            token.encode(),
            self.connection_drain_deadline(),
        );
    }

    fn schedule_connection_drain_now(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) {
        connections.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            token.encode(),
            std::time::Duration::ZERO,
        );
    }

    pub(in crate::v2::engine) fn handle_connection_drain_deadline_into(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut super::super::io_core::IoState,
        token: ConnectionToken,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let Lookup::Occupied(_) = connections.lookup(token) else {
            return;
        };
        debug_assert!(
            actions.can_accept(2),
            "connection drain deadline must reserve its tail publications before dequeue"
        );
        if connections.close_started(token) && !self.shutdown_requested() {
            let scan_was_complete = connections.close_scan(token).is_some_and(|(_, done)| done);
            if !self.scan_close_observers_into(io_core, connections, token, 2, actions) {
                self.schedule_connection_drain_now(connections, token);
                return;
            }
            if !scan_was_complete {
                // The zero-delay continuation finished only observer
                // publication. Preserve the configured grace period before
                // any destructive QP fallback.
                self.schedule_connection_drain(connections, token);
                return;
            }
        }
        // CQEs already copied out of the hardware CQ must take the ordinary
        // quantum-bounded ready path before a destructive fallback can run.
        if connections.has_completion_work(token) {
            io_core.publish_connection(token);
            connections.schedule_deadline(
                DeadlineKind::ConnectionDrain,
                token.encode(),
                std::time::Duration::ZERO,
            );
            return;
        }
        if connections.is_quarantined(token) {
            if !self.scan_quarantine_operations_into(connections, io_core, token, actions) {
                self.schedule_connection_drain_now(connections, token);
            }
            return;
        }
        let accepted_count = connections.accepted_count(token);
        let accepted_tokens = connections
            .accepted_tokens_bounded(token, actions.remaining().saturating_sub(2).min(32));
        if accepted_count != 0 && accepted_tokens.is_empty() {
            self.schedule_connection_drain_now(connections, token);
            return;
        }
        let mut reclamation_proof = connections.take_qp_reclamation_proof(token);
        if !accepted_tokens.is_empty() && reclamation_proof.is_none() {
            let _admission = read_unpoison(&self.frontend.admission);
            reclamation_proof = match self.establish_qp_destruction_proof(connections, token) {
                Ok(proof) => Some(proof),
                Err(error) => {
                    let qp_num = connections
                        .with_connection(token, |connection| connection.qp_num())
                        .unwrap_or(0);
                    tracing::warn!(
                        qp_num,
                        %error,
                        "failed to establish result-aware QP destruction boundary"
                    );
                    None
                }
            };
        }
        if let Some(proof) = reclamation_proof {
            let prefix = io_core.qp_destroy_publication_prefix(
                &accepted_tokens,
                actions.remaining().saturating_sub(2),
            );
            if prefix == 0 && !accepted_tokens.is_empty() {
                connections.store_qp_reclamation_proof(token, proof);
                self.schedule_connection_drain_now(connections, token);
                return;
            }
            self.reclaim_after_qp_destroy_into(
                connections,
                io_core,
                &proof,
                token,
                accepted_tokens[..prefix].to_vec(),
                actions,
            );
            // Every snapshotted token was attempted. Any survivor is an
            // anomalous accepted-set entry and follows the established
            // complete-bundle quarantine path below.
            if connections.accepted_count(token) != 0
                && (prefix < accepted_tokens.len() || accepted_tokens.len() < accepted_count)
            {
                connections.store_qp_reclamation_proof(token, proof);
                self.schedule_connection_drain_now(connections, token);
                return;
            }
            if self.reject_queued_completions_after_qp_destroy_into(
                connections,
                io_core,
                token,
                actions,
            ) {
                io_core.publish_connection(token);
                connections.schedule_deadline(
                    DeadlineKind::ConnectionDrain,
                    token.encode(),
                    std::time::Duration::ZERO,
                );
                return;
            }
        }
        let report = connections
            .with_connection_mut(token, |connection| connection.begin_quarantine(io_core))
            .flatten();
        if let Some(report) = report {
            self.track_connection_quarantine(connections, token);
            if let Some(event) = connections
                .with_connection_mut(token, |connection| {
                    connection.publish_quarantine_into(
                        report.outstanding_operations,
                        report.cq_debt,
                        actions,
                    )
                })
                .flatten()
            {
                actions.push_event(event);
            }
            if !self.scan_quarantine_operations_into(connections, io_core, token, actions) {
                self.schedule_connection_drain_now(connections, token);
                return;
            }
        }

        if connections.close_started(token) && connections.accepted_count(token) == 0 {
            self.recover_connection_quarantine(connections, token);
            self.record_connection_drained(connections, token);
            self.schedule_connection_retirement(connections, token);
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn handle_connection_drain_deadline(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut super::super::io_core::IoState,
        token: ConnectionToken,
    ) {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        self.handle_connection_drain_deadline_into(connections, io_core, token, &mut actions);
        actions.publish();
    }

    pub(in crate::v2::engine) fn recover_connection_quarantine(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) {
        self.recover_connection_quarantine_entry(connections, token);
    }

    pub(in crate::v2::engine) fn record_connection_drained(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) {
        connections.mark_drained_once(token);
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

    use super::super::super::io_core::{
        install_accepted_operation_for_driver_test, register_operation_waker_for_test,
    };
    use super::super::super::registry::OperationToken;
    use super::super::super::{
        CompletionMode, RdmaConnectionConfig, RdmaEngineLifecycle, test_engine_pair,
    };
    use super::super::connection::{WorkRequestPoster, install_connection};
    use crate::v2::error::{Error, Result};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};
    use futures_util::task::{ArcWake, waker};

    struct GuardCheckingWaker {
        session: Arc<super::super::SessionFrontend>,
        wakes: AtomicUsize,
        lock_failures: AtomicUsize,
    }

    impl ArcWake for GuardCheckingWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.wakes.fetch_add(1, Ordering::AcqRel);
            let admission_unlocked = arc_self.session.admission.try_write().is_ok();
            if !admission_unlocked {
                arc_self.lock_failures.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

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

        fn to_error(
            &self,
            _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
        ) -> Result<()> {
            self.error_transitions.fetch_add(1, Ordering::AcqRel);
            if self.fail_error_transition {
                Err(Error::Verbs(std::io::Error::other(
                    "injected QP ERR transition failure",
                )))
            } else {
                Ok(())
            }
        }

        fn destroy_qp(
            &self,
            _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
        ) -> Result<bool> {
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
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = TestPoster::failing(19);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            poster,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let mut close = Box::pin(connection.close());
        assert!(poll_once(close.as_mut()).is_pending());
        assert_eq!(
            driver
                .reactor
                .session
                .connections
                .admission_snapshot_excluding_retained()
                .draining,
            0
        );
        engine.shared.commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );

        let driver_error = loop {
            match poll_once(Pin::new(&mut driver)) {
                Poll::Ready(Err(error)) => break error,
                Poll::Ready(Ok(())) => panic!("QP transition failure completed successfully"),
                Poll::Pending => {}
            }
        };
        let Poll::Ready(Err(close_error)) = poll_once(close.as_mut()) else {
            panic!("bounded terminal cleanup did not wake the connection close");
        };
        assert!(
            matches!(driver_error, Error::Verbs(_)),
            "unexpected driver error: {driver_error:?}"
        );
        assert_eq!(close_error.to_string(), driver_error.to_string());
        assert_eq!(
            driver
                .reactor
                .session
                .connections
                .admission_snapshot_excluding_retained()
                .draining,
            0
        );
        assert_eq!(engine.diagnostics().live_connections, 1);
        assert_eq!(engine.diagnostics().lifecycle, RdmaEngineLifecycle::Failed);
        drop(driver);
    }

    #[test]
    fn failed_error_transition_wakes_after_admission_guard_drops() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            TestPoster::failing(21),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let reentrant = Arc::new(GuardCheckingWaker {
            session: Arc::clone(&engine.shared.session),
            wakes: AtomicUsize::new(0),
            lock_failures: AtomicUsize::new(0),
        });
        let task_waker = waker(Arc::clone(&reentrant));
        let notify = connection.close_state().notify();
        let mut notified = Box::pin(notify.notified());
        assert!(
            notified
                .as_mut()
                .poll(&mut Context::from_waker(&task_waker))
                .is_pending()
        );

        driver.reactor.session.manager.begin_connection_close(
            &mut driver.reactor.session.connections,
            driver.reactor.io.core_mut(),
            connection.session_token(),
        );

        assert!(
            reentrant.wakes.load(Ordering::Acquire) >= 1,
            "failed transition must wake the registered close waiter"
        );
        assert_eq!(
            reentrant.lock_failures.load(Ordering::Acquire),
            0,
            "transition-failure wake must run after the admission guard drops"
        );
        assert_eq!(
            driver
                .reactor
                .session
                .connections
                .admission_snapshot()
                .draining,
            0,
            "failed transition must still roll back the draining gauge"
        );
        assert!(matches!(
            connection
                .close_state()
                .raw_outcome()
                .unwrap()
                .into_result(),
            Err(Error::Verbs(_))
        ));
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        future.poll(&mut context)
    }

    #[test]
    fn connection_close_wakes_driver_after_backend_commit() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            TestPoster::new(20),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let reentrant = Arc::new(GuardCheckingWaker {
            session: Arc::clone(&engine.shared.session),
            wakes: AtomicUsize::new(0),
            lock_failures: AtomicUsize::new(0),
        });
        let task_waker = waker(Arc::clone(&reentrant));
        engine
            .shared
            .work_signal
            .register_waker_for_test(&task_waker);

        driver.reactor.session.manager.begin_connection_close(
            &mut driver.reactor.session.connections,
            driver.reactor.io.core_mut(),
            connection.session_token(),
        );

        assert!(
            reentrant.wakes.load(Ordering::Acquire) >= 1,
            "connection close must publish I/O work to the registered driver waker"
        );
        assert_eq!(
            reentrant.lock_failures.load(Ordering::Acquire),
            0,
            "connection-close publication must run after backend state commits"
        );
    }

    #[test]
    fn connection_close_wakes_operation_observer_after_backend_commit() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            TestPoster::new(22),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let token = install_accepted_operation_for_driver_test(
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session.connections,
            connection.session_token(),
            crate::wc::WcOpcode::Send,
        );
        let reentrant = Arc::new(GuardCheckingWaker {
            session: Arc::clone(&engine.shared.session),
            wakes: AtomicUsize::new(0),
            lock_failures: AtomicUsize::new(0),
        });
        register_operation_waker_for_test(
            driver.reactor.io.core(),
            token,
            &waker(Arc::clone(&reentrant)),
        );

        driver.reactor.session.manager.begin_connection_close(
            &mut driver.reactor.session.connections,
            driver.reactor.io.core_mut(),
            connection.session_token(),
        );

        assert_eq!(
            reentrant.wakes.load(Ordering::Acquire),
            1,
            "accepted operation observer must be detached and woken once"
        );
        assert_eq!(
            reentrant.lock_failures.load(Ordering::Acquire),
            0,
            "operation-observer wake must run after backend state commits"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_close_scan_continuation_preserves_drain_grace() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = TestPoster::new(28);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default().max_send_wr(64),
            None,
            None,
        )
        .unwrap();
        for _ in 0..33 {
            install_accepted_operation_for_driver_test(
                driver.reactor.io.core_mut(),
                &mut driver.reactor.session.connections,
                connection.session_token(),
                crate::wc::WcOpcode::Send,
            );
        }

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        driver.reactor.session.manager.begin_connection_close_into(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            connection.session_token(),
            driver.reactor.io.core_mut(),
            &mut actions,
        );
        actions.publish();
        let immediate = driver.reactor.session.connections.take_deadline_requests(1);
        assert_eq!(immediate.len(), 1);
        assert_eq!(immediate[0].at, tokio::time::Instant::now());

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        driver
            .reactor
            .session
            .manager
            .handle_connection_drain_deadline_into(
                &mut driver.reactor.session.connections,
                driver.reactor.io.core_mut(),
                connection.session_token(),
                &mut actions,
            );
        actions.publish();
        let grace = driver.reactor.session.connections.take_deadline_requests(1);
        assert_eq!(grace.len(), 1);
        assert!(
            grace[0].at > tokio::time::Instant::now(),
            "finishing the observer scan must arm the configured drain grace"
        );
        assert_eq!(
            poster.destroys.load(Ordering::Acquire),
            0,
            "observer-scan continuation cannot destroy the QP"
        );
    }

    fn install_accepted_connection(
        _engine: &super::super::super::RdmaEngine,
        driver: &mut super::super::super::RdmaEngineDriver,
        qp_num: u32,
    ) -> (
        super::super::super::RdmaConnection,
        Arc<TestPoster>,
        OperationToken,
    ) {
        let poster = TestPoster::new(qp_num);
        let poster_dyn: Arc<dyn WorkRequestPoster> = poster.clone();
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
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
        let connection_token = connection.session_token();
        let (io, session) = (&mut driver.reactor.io, &mut driver.reactor.session);
        session
            .connections
            .with_connection_io_mut(connection_token, |connection, connection_io, _poster| {
                io.core_mut().add_accepted(connection, connection_io, token);
            })
            .unwrap();
        driver.reactor.io.core_mut().accepted_operations += 1;
        (connection, poster, token)
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_operation_ownership_quarantines_until_exact_removal() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let (connection, poster, token) = install_accepted_connection(&engine, &mut driver, 17);
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
            driver
                .reactor
                .session
                .connections
                .admission_snapshot()
                .registered_live_qps,
            0
        );
        assert_eq!(
            poster.destroys.load(Ordering::Acquire),
            1,
            "the defensive unknown-token branch retains accounting after a safe QP boundary"
        );

        let connection_token = connection.session_token();
        let (io, session) = (&mut driver.reactor.io, &mut driver.reactor.session);
        assert!(
            session
                .connections
                .with_connection_io_mut(connection_token, |connection, connection_io, _poster| {
                    io.core_mut()
                        .remove_accepted(connection, connection_io, token)
                },)
                .unwrap()
        );
        driver.reactor.io.core_mut().accepted_operations -= 1;
        driver
            .reactor
            .session
            .manager
            .recover_connection_quarantine(
                &mut driver.reactor.session.connections,
                connection_token,
            );
        assert_eq!(
            driver
                .reactor
                .session
                .connections
                .admission_snapshot()
                .registered_live_qps,
            0
        );
        driver
            .reactor
            .session
            .manager
            .record_connection_drained(&mut driver.reactor.session.connections, connection_token);
        driver
            .reactor
            .session
            .manager
            .schedule_connection_retirement(
                &mut driver.reactor.session.connections,
                connection_token,
            );
        driver
            .reactor
            .session
            .cm
            .service_software(
                &mut driver.reactor.session.connections,
                &driver.reactor.session.manager,
                driver.reactor.io.core_mut(),
                None,
                32,
            )
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
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        install_accepted_operation_for_driver_test(
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session.connections,
            connection.session_token(),
            crate::wc::WcOpcode::Recv,
        );
        let mut close = Box::pin(connection.close());

        assert!(poll_once(close.as_mut()).is_pending());
        assert!(poll_once(Pin::new(&mut driver)).is_pending());
        tokio::time::advance(Duration::from_secs(5)).await;
        for _ in 0..4 {
            assert!(poll_once(Pin::new(&mut driver)).is_pending());
            if poster.destroys.load(Ordering::Acquire) == 1 {
                break;
            }
        }
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
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let poster = TestPoster::new(24);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        let connection_token = connection.session_token();
        for opcode in [crate::wc::WcOpcode::Send, crate::wc::WcOpcode::Recv] {
            install_accepted_operation_for_driver_test(
                driver.reactor.io.core_mut(),
                &mut driver.reactor.session.connections,
                connection_token,
                opcode,
            );
        }
        let anomalous = OperationToken {
            slot: u32::MAX,
            generation: 1,
        };
        let (io, session) = (&mut driver.reactor.io, &mut driver.reactor.session);
        session
            .connections
            .with_connection_io_mut(connection_token, |connection, connection_io, _poster| {
                io.core_mut()
                    .add_accepted(connection, connection_io, anomalous);
            })
            .unwrap();
        driver.reactor.io.core_mut().accepted_operations += 1;
        driver
            .reactor
            .session
            .connections
            .begin_close(connection_token);
        driver
            .reactor
            .session
            .manager
            .transition_connection_to_error(
                &mut driver.reactor.session.connections,
                connection_token,
            )
            .unwrap();

        driver
            .reactor
            .session
            .manager
            .handle_connection_drain_deadline(
                &mut driver.reactor.session.connections,
                driver.reactor.io.core_mut(),
                connection_token,
            );
        driver
            .reactor
            .session
            .manager
            .handle_connection_drain_deadline(
                &mut driver.reactor.session.connections,
                driver.reactor.io.core_mut(),
                connection_token,
            );

        assert_eq!(poster.destroys.load(Ordering::Acquire), 1);
        assert_eq!(
            driver.reactor.io.core().diagnostics().registered_operations,
            0
        );
        assert_eq!(driver.reactor.io.core().accepted_count(), 1);
        assert_eq!(
            driver
                .reactor
                .session
                .connections
                .accepted_tokens_bounded(connection_token, usize::MAX),
            vec![anomalous]
        );
        assert_eq!(
            driver
                .reactor
                .session
                .connections
                .admission_snapshot()
                .quarantined_bundles,
            1
        );
    }
}
