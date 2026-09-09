//! Bounded CQ, completion-dispatch, and operation-reclamation progress.

use std::collections::{HashSet, VecDeque};
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tokio::time::Instant;

#[cfg(test)]
use super::IoCore;
#[cfg(test)]
use super::IoDeadlineRequest;
#[cfg(test)]
use super::IoSessionBridge;
use super::IoState;
use crate::v2::Completion;
use crate::v2::completion::CqReadiness;
use crate::v2::engine::config::CompletionMode;
#[cfg(test)]
use crate::v2::engine::progress::ProgressReport;
use crate::v2::engine::progress::ReadinessRegistration;
use crate::v2::engine::registry::{ConnectionToken, OperationToken};
use crate::v2::engine::resources::EngineReactorResources;
use crate::v2::engine::scheduler::DeadlineQueue;
use crate::v2::error::{Error, Result};

/// I/O-owned progress state. It has no concrete dependency on session state.
pub(in crate::v2::engine) struct IoReactorSources {
    core: Option<IoState>,
    #[cfg(test)]
    bridge: Arc<dyn IoSessionBridge>,
    cq_readiness: CqReadiness,
    cq_buffer: Box<[Completion]>,
    completion_connections: CompletionConnections,
    deadlines: DeadlineQueue<OperationToken>,
    ready_deadlines: VecDeque<OperationToken>,
    completion_dispatch_budget: usize,
    reclamation_budget: usize,
    terminal_cursor: usize,
    terminal_complete: bool,
    #[cfg(test)]
    turns: usize,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: Arc<crate::v2::engine::driver::test_api::TestDriverState>,
}

impl IoReactorSources {
    pub(in crate::v2::engine) fn new(
        core: IoState,
        #[cfg(test)] bridge: Arc<dyn IoSessionBridge>,
        cq_budget: usize,
        completion_dispatch_budget: usize,
        reclamation_budget: usize,
        #[cfg(any(test, feature = "test-hooks"))] test_driver: Arc<
            crate::v2::engine::driver::test_api::TestDriverState,
        >,
    ) -> Self {
        Self {
            core: Some(core),
            #[cfg(test)]
            bridge,
            cq_readiness: CqReadiness::default(),
            cq_buffer: vec![Completion::default(); cq_budget].into_boxed_slice(),
            completion_connections: CompletionConnections::default(),
            deadlines: DeadlineQueue::default(),
            ready_deadlines: VecDeque::new(),
            completion_dispatch_budget,
            reclamation_budget,
            terminal_cursor: 0,
            terminal_complete: false,
            #[cfg(test)]
            turns: 0,
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn turn(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> Result<ProgressReport> {
        #[cfg(test)]
        {
            self.turns = self.turns.saturating_add(1);
        }
        if let Some(outcome) = self.core().terminal_failure() {
            let budget = self
                .cq_buffer
                .len()
                .saturating_add(self.completion_dispatch_budget)
                .saturating_add(self.reclamation_budget);
            let cursor = self.terminal_cursor;
            let (effects, next, complete, scanned) = self
                .core_mut()
                .terminalize_operations_bounded(&outcome, cursor, budget);
            self.terminal_cursor = next;
            self.terminal_complete = complete;
            self.bridge.commit_terminal_effects(effects);
            return Ok(ProgressReport::running(
                scanned,
                !complete,
                ReadinessRegistration::NotRequired,
            ));
        }
        let (cq_units, readiness, cq_repoll) = self.service_cq_for_test(mode, cx)?;
        let (reclamation_units, reclamation_ready) = self.service_reclamation_for_test()?;
        let actions = crate::v2::engine::reactor::ReactorActions::default();
        let (dispatch_units, dispatch_ready) =
            self.service_completion_dispatch_for_test(self.completion_dispatch_budget)?;
        actions.publish();
        let units_consumed = cq_units
            .saturating_add(reclamation_units)
            .saturating_add(dispatch_units);
        Ok(ProgressReport::running(
            units_consumed,
            cq_repoll || reclamation_ready || dispatch_ready,
            readiness,
        ))
    }

    pub(in crate::v2::engine) fn begin_turn(&mut self) -> Result<bool> {
        #[cfg(test)]
        {
            self.turns = self.turns.saturating_add(1);
        }

        if self.core().terminal_failure().is_some() {
            return Ok(true);
        }
        Ok(false)
    }

    pub(in crate::v2::engine) fn sync_lifecycle(
        &mut self,
        admission_error: Option<Error>,
        terminal: Option<crate::v2::engine::lifecycle::MemoizedTerminalResult>,
    ) {
        if admission_error.is_some() {
            self.core_mut().close_admission(admission_error);
        }
        if let Some(outcome) = terminal {
            self.core_mut().begin_terminal_failure(outcome);
        }
    }

    pub(in crate::v2::engine) fn diagnostics(&self) -> super::IoCoreDiagnostics {
        self.core().diagnostics()
    }

    pub(in crate::v2::engine) fn service_terminal(
        &mut self,
        budget: usize,
        session: &mut crate::v2::engine::session::SessionReactorSources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> (usize, bool) {
        let Some(outcome) = self.core().terminal_failure() else {
            return (0, true);
        };
        let cursor = self.terminal_cursor;
        let (effects, next, complete, scanned) = self
            .core_mut()
            .terminalize_operations_bounded(&outcome, cursor, budget);
        self.terminal_cursor = next;
        self.terminal_complete = complete;
        session.commit_io_effects_into(effects, actions);
        (scanned, complete)
    }

    pub(in crate::v2::engine) fn cq_budget(&self) -> usize {
        self.cq_buffer.len()
    }

    pub(in crate::v2::engine) fn completion_dispatch_budget(&self) -> usize {
        self.completion_dispatch_budget
    }

    pub(in crate::v2::engine) fn reclamation_budget(&self) -> usize {
        self.reclamation_budget
    }

    pub(in crate::v2::engine) fn reclamation_request_count(&self) -> usize {
        self.core().reclamation_request_count()
    }

    pub(in crate::v2::engine) fn due_deadline_count(&self, now: Instant) -> usize {
        self.ready_deadlines
            .len()
            .saturating_add(usize::from(self.deadlines.has_due(now)))
    }

    pub(in crate::v2::engine) fn prepare_due_deadline_snapshot(&mut self, now: Instant) -> usize {
        let available = self
            .reclamation_budget
            .saturating_sub(self.ready_deadlines.len());
        self.ready_deadlines
            .extend(self.deadlines.drain_due(now, available));
        self.ready_deadlines.len()
    }

    pub(in crate::v2::engine) fn completion_source_count(&self) -> usize {
        self.completion_connections
            .len()
            .saturating_add(self.core().published_connection_count())
    }

    pub(in crate::v2::engine) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.next()
    }

    pub(in crate::v2::engine) fn can_finish(&self) -> bool {
        if self.core().terminal_failure().is_some() {
            return self.terminal_complete;
        }
        self.core().shutdown_requested() && self.core().accepted_count() == 0
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn completion_connection_count(&self) -> usize {
        self.completion_connections.len()
    }

    pub(in crate::v2::engine) fn prepare_completion_dispatch_snapshot(&mut self) -> usize {
        if let Some(connection) = self.core_mut().take_published_connection() {
            self.enqueue_connection(connection);
        }
        self.completion_connections.len()
    }

    #[cfg(test)]
    fn cq_buffer_capacity(&self) -> usize {
        self.cq_buffer.len()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn turn_count(&self) -> usize {
        self.turns
    }

    pub(in crate::v2::engine) fn core(&self) -> &IoState {
        self.core
            .as_ref()
            .expect("driver-owned I/O state is available while polling")
    }

    pub(in crate::v2::engine) fn core_mut(&mut self) -> &mut IoState {
        self.core
            .as_mut()
            .expect("driver-owned I/O state is available while polling")
    }

    fn enqueue_connection(&mut self, connection: ConnectionToken) {
        self.completion_connections.enqueue(connection);
    }

    pub(in crate::v2::engine) fn service_cq(
        &mut self,
        session: &mut crate::v2::engine::session::SessionReactorSources,
        resources: Option<&EngineReactorResources>,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> Result<(usize, ReadinessRegistration, bool)> {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(completion) = self.test_driver.take_released_connection_cqe() {
            if let Some(connection) =
                session.enqueue_completion_with_core(self.core_mut(), completion)
            {
                self.enqueue_connection(connection);
            }
            return Ok((1, ReadinessRegistration::Incomplete, true));
        }

        let Some(resources) = resources else {
            return Ok((0, ReadinessRegistration::NotRequired, false));
        };
        let (count, readiness) = match mode {
            CompletionMode::Readiness => {
                let async_fd = resources.cq_async_fd.as_ref().ok_or_else(|| {
                    Error::InvalidConfig("readiness engine has no CQ AsyncFd".into())
                })?;
                #[cfg(any(test, feature = "test-hooks"))]
                let polled = {
                    let before = Arc::clone(&self.test_driver);
                    let after = Arc::clone(&self.test_driver);
                    self.cq_readiness.poll_with_async_fd_and_hooks(
                        &resources.cq,
                        async_fd,
                        cx,
                        &mut self.cq_buffer,
                        move |generation| before.record_cq_pre_arm(generation),
                        move |generation| after.record_cq_arm(generation),
                    )
                };
                #[cfg(not(any(test, feature = "test-hooks")))]
                let polled = self.cq_readiness.poll_with_async_fd_and_hooks(
                    &resources.cq,
                    async_fd,
                    cx,
                    &mut self.cq_buffer,
                    |_| false,
                    |_| false,
                );
                match polled {
                    Poll::Ready(result) => (result?, ReadinessRegistration::Incomplete),
                    Poll::Pending if self.cq_readiness.requires_repoll() => {
                        (0, ReadinessRegistration::Incomplete)
                    }
                    Poll::Pending => (0, ReadinessRegistration::RegisteredAndRechecked),
                }
            }
            CompletionMode::Polling => (
                resources.cq.poll(&mut self.cq_buffer)?,
                ReadinessRegistration::NotRequired,
            ),
        };

        if count == 0 {
            return Ok((0, readiness, false));
        }
        let completions = self.cq_buffer[..count].to_vec();
        for completion in completions {
            let completion = completion.into_raw();
            #[cfg(any(test, feature = "test-hooks"))]
            if self.test_driver.suppress_connection_cqe(completion) {
                continue;
            }
            if let Some(connection) =
                session.enqueue_completion_with_core(self.core_mut(), completion)
            {
                self.enqueue_connection(connection);
            }
            #[cfg(any(test, feature = "test-hooks"))]
            self.test_driver.dispatch(completion);
            #[cfg(not(any(test, feature = "test-hooks")))]
            let _ = completion;
        }
        Ok((count, readiness, true))
    }

    #[cfg(test)]
    fn service_cq_for_test(
        &mut self,
        _mode: CompletionMode,
        _cx: &mut TaskContext<'_>,
    ) -> Result<(usize, ReadinessRegistration, bool)> {
        if let Some(completion) = self.test_driver.take_released_connection_cqe() {
            let bridge = Arc::clone(&self.bridge);
            if let Some(connection) = bridge.route_completion(self.core_mut(), completion) {
                self.enqueue_connection(connection);
            }
            return Ok((1, ReadinessRegistration::Incomplete, true));
        }
        Ok((0, ReadinessRegistration::NotRequired, false))
    }

    #[cfg(test)]
    fn service_reclamation_for_test(&mut self) -> Result<(usize, bool)> {
        let now = Instant::now();
        let request_limit = self
            .reclamation_request_count()
            .min(self.reclamation_budget);
        let requests = self.service_reclamation_requests(request_limit)?;
        let actions = crate::v2::engine::reactor::ReactorActions::default();
        let deadlines =
            self.service_reclamation_deadlines_for_test(now, self.reclamation_budget - requests);
        actions.publish();
        Ok((
            requests + deadlines,
            self.core().has_reclamation_requests()
                || self.deadlines.next().is_some_and(|at| at <= now),
        ))
    }

    pub(in crate::v2::engine) fn service_reclamation_requests(
        &mut self,
        limit: usize,
    ) -> Result<usize> {
        let mut consumed = 0;
        for request in self.core_mut().take_reclamation_requests(limit) {
            self.deadlines
                .push(request.at, request.token)
                .map_err(|_| {
                    Error::InvalidConfig("I/O deadline insertion sequence exhausted".into())
                })?;
            consumed += 1;
        }
        Ok(consumed)
    }

    pub(in crate::v2::engine) fn service_reclamation_deadlines(
        &mut self,
        _now: Instant,
        limit: usize,
        session: &mut crate::v2::engine::session::SessionReactorSources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> usize {
        let mut consumed = 0;
        while consumed < limit {
            let Some(token) = self.ready_deadlines.pop_front() else {
                break;
            };
            session.handle_reclamation_deadline_with_core(self.core_mut(), token, actions);
            consumed += 1;
        }
        consumed
    }

    #[cfg(test)]
    fn service_reclamation_deadlines_for_test(&mut self, _now: Instant, limit: usize) -> usize {
        let mut consumed = 0;
        while consumed < limit {
            let Some(token) = self.ready_deadlines.pop_front() else {
                break;
            };
            let bridge = Arc::clone(&self.bridge);
            bridge.handle_reclamation_deadline(self.core_mut(), token);
            consumed += 1;
        }
        consumed
    }

    pub(in crate::v2::engine) fn service_completion_dispatch(
        &mut self,
        quantum: usize,
        session: &mut crate::v2::engine::session::SessionReactorSources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<(usize, bool)> {
        let Some(connection) = self.completion_connections.pop() else {
            return Ok((0, self.core().has_published_connections()));
        };
        let (processed, remains_ready) = session.dispatch_connection_completions_with_core(
            self.core_mut(),
            connection,
            quantum,
            actions,
        );
        if remains_ready {
            self.enqueue_connection(connection);
        }
        Ok((
            processed,
            self.completion_connections.len() > 0 || self.core().has_published_connections(),
        ))
    }

    #[cfg(test)]
    fn service_completion_dispatch_for_test(&mut self, quantum: usize) -> Result<(usize, bool)> {
        let Some(connection) = self.completion_connections.pop() else {
            return Ok((0, self.core().has_published_connections()));
        };
        let bridge = Arc::clone(&self.bridge);
        let (processed, remains_ready) =
            bridge.dispatch_connection_completions(self.core_mut(), connection, quantum);
        if remains_ready {
            self.enqueue_connection(connection);
        }
        Ok((
            processed,
            self.completion_connections.len() > 0 || self.core().has_published_connections(),
        ))
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn schedule_deadline_for_test(
        &mut self,
        at: Instant,
        token: OperationToken,
    ) {
        self.deadlines
            .push(at, token)
            .expect("test I/O deadline insertion");
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn clear_deadlines_for_test(&mut self) {
        self.deadlines.clear();
    }
}

#[derive(Default)]
struct CompletionConnections {
    queue: VecDeque<ConnectionToken>,
    queued: HashSet<ConnectionToken>,
}

impl CompletionConnections {
    fn enqueue(&mut self, connection: ConnectionToken) {
        if self.queued.insert(connection) {
            self.queue.push_back(connection);
        }
    }

    fn pop(&mut self) -> Option<ConnectionToken> {
        let connection = self.queue.pop_front()?;
        self.queued.remove(&connection);
        Some(connection)
    }

    fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, RwLock};
    use std::task::Context as TaskContext;
    use std::time::Duration;

    use super::*;
    use crate::v2::engine::driver::test_api::TestDriverState;
    use crate::v2::engine::io_core::{
        EstablishedIoConnection, EstablishedIoIdentity, IoDriverSignal,
        operation_future_for_io_lifetime_test,
    };
    use crate::v2::engine::registry::lock_unpoison;

    struct NoopSignal;

    impl IoDriverSignal for NoopSignal {
        fn publish_cq_recheck(&self) {}
        fn publish_completion_dispatch(&self) {}
        fn publish_reclamation(&self) {}
        fn pause_operation_before_register(&self) {}
    }

    #[derive(Default)]
    struct RecordingBridge {
        dispatched: Mutex<Vec<(ConnectionToken, usize)>>,
        reclaimed: Mutex<Vec<OperationToken>>,
        remains_ready: AtomicBool,
    }

    impl IoSessionBridge for RecordingBridge {
        fn route_completion(
            &self,
            _io: &mut IoState,
            _completion: crate::wc::WorkCompletion,
        ) -> Option<ConnectionToken> {
            None
        }

        fn dispatch_connection_completions(
            &self,
            _io: &mut IoState,
            connection: ConnectionToken,
            quantum: usize,
        ) -> (usize, bool) {
            lock_unpoison(&self.dispatched).push((connection, quantum));
            (
                quantum,
                self.remains_ready
                    .load(std::sync::atomic::Ordering::Acquire),
            )
        }

        fn handle_reclamation_deadline(&self, _io: &mut IoState, token: OperationToken) {
            lock_unpoison(&self.reclaimed).push(token);
        }

        fn commit_terminal_effects(&self, _effects: super::super::IoCoreEffects) {}
    }

    fn progress(reclamation_budget: usize) -> (IoReactorSources, Arc<RecordingBridge>) {
        let signal: Arc<dyn IoDriverSignal> = Arc::new(NoopSignal);
        let core = IoCore::new_owned(
            16,
            16,
            Duration::from_secs(1),
            3,
            Arc::new(RwLock::new(())),
            signal,
        )
        .unwrap();
        let bridge = Arc::new(RecordingBridge::default());
        let bridge_dyn: Arc<dyn IoSessionBridge> = bridge.clone();
        let progress = IoReactorSources::new(
            core,
            bridge_dyn,
            4,
            3,
            reclamation_budget,
            Arc::new(TestDriverState::new()),
        );
        (progress, bridge)
    }

    fn service_due(progress: &mut IoReactorSources, now: Instant, limit: usize) -> usize {
        progress.prepare_due_deadline_snapshot(now);
        progress.service_reclamation_deadlines_for_test(now, limit)
    }

    fn connection(slot: u32) -> ConnectionToken {
        ConnectionToken {
            slot,
            generation: 1,
        }
    }

    #[test]
    fn completion_connections_deduplicate_and_rotate() {
        let mut connections = CompletionConnections::default();
        connections.enqueue(connection(1));
        connections.enqueue(connection(1));
        connections.enqueue(connection(2));

        assert_eq!(connections.len(), 2);
        let first = connections.pop().unwrap();
        connections.enqueue(first);
        assert_eq!(connections.pop(), Some(connection(2)));
        assert_eq!(connections.pop(), Some(connection(1)));
    }

    #[test]
    fn operation_future_outlives_progress_without_retaining_session_bridge() {
        let (mut progress, bridge) = progress(1);
        let bridge_weak = Arc::downgrade(&bridge);
        let connection = EstablishedIoConnection::new(
            EstablishedIoIdentity {
                connection: connection(7),
                qp_num: 7,
            },
            1,
            1,
            Arc::new(tokio::sync::Notify::new()),
        );
        let mut connection_io = super::super::ConnectionIoState::from_connection(&connection);
        let operation = operation_future_for_io_lifetime_test(
            progress.core_mut(),
            &connection,
            &mut connection_io,
        );

        drop(connection);
        drop(bridge);
        drop(progress);

        assert!(
            bridge_weak.upgrade().is_none(),
            "an operation future must not retain the session progress bridge"
        );
        drop(operation);
    }

    #[test]
    fn operation_deadlines_are_ordered_and_budgetable() {
        let now = Instant::now();
        let mut deadlines = DeadlineQueue::default();
        deadlines
            .push(now + Duration::from_secs(2), OperationToken::decode(2))
            .unwrap();
        deadlines.push(now, OperationToken::decode(0)).unwrap();
        deadlines.push(now, OperationToken::decode(1)).unwrap();

        assert_eq!(deadlines.pop_one_due(now), Some(OperationToken::decode(0)));
        assert_eq!(deadlines.pop_one_due(now), Some(OperationToken::decode(1)));
        assert_eq!(deadlines.next(), Some(now + Duration::from_secs(2)));
    }

    #[test]
    fn deadline_sequence_exhaustion_maps_to_io_configuration_error() {
        let (mut progress, _bridge) = progress(1);
        progress.deadlines.exhaust_sequence_for_test();
        progress
            .core_mut()
            .reclamation_requests
            .push_back(IoDeadlineRequest {
                at: Instant::now(),
                token: OperationToken::decode(1),
            });
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);

        let error = match progress.turn(CompletionMode::Polling, &mut cx) {
            Ok(_) => panic!("exhausted I/O deadline sequence must fail"),
            Err(error) => error,
        };

        assert!(matches!(error, Error::InvalidConfig(message) if message.contains("I/O deadline")));
    }

    #[test]
    fn owner_turn_bounds_one_connection_and_reports_remaining_work() {
        let (mut progress, bridge) = progress(1);
        progress.enqueue_connection(connection(1));
        progress.enqueue_connection(connection(2));
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);

        let report = progress.turn(CompletionMode::Polling, &mut cx).unwrap();

        assert_eq!(progress.cq_buffer_capacity(), 4);
        assert_eq!(
            lock_unpoison(&bridge.dispatched).as_slice(),
            &[(connection(1), 3)]
        );
        assert_eq!(report.units_consumed, 3);
        assert!(report.immediate_work);
        assert_eq!(report.readiness, ReadinessRegistration::NotRequired);
    }

    #[test]
    fn reclamation_sources_are_independently_bounded() {
        let (mut progress, bridge) = progress(1);
        let now = Instant::now();
        progress.core_mut().reclamation_requests.extend([
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(1),
            },
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(2),
            },
        ]);
        assert_eq!(progress.service_reclamation_requests(1).unwrap(), 1);
        assert!(lock_unpoison(&bridge.reclaimed).is_empty());

        assert_eq!(service_due(&mut progress, now, 1), 1);
        assert_eq!(
            lock_unpoison(&bridge.reclaimed).as_slice(),
            &[OperationToken::decode(1)]
        );
    }

    #[test]
    fn split_reclamation_sources_preserve_the_aggregate_budget() {
        let (mut progress, _bridge) = progress(2);
        let now = Instant::now();
        progress
            .core_mut()
            .reclamation_requests
            .push_back(IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(1),
            });
        let requests = progress.service_reclamation_requests(1).unwrap();
        let deadlines = service_due(&mut progress, now, 1);
        assert_eq!(requests + deadlines, 2);
    }

    #[test]
    fn sustained_io_sources_alternate_and_leave_bounded_work_for_next_turn() {
        let (mut progress, bridge) = progress(4);
        let now = Instant::now();
        progress.schedule_deadline_for_test(now, OperationToken::decode(10));
        progress.schedule_deadline_for_test(now, OperationToken::decode(11));
        progress.core_mut().reclamation_requests.extend([
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(1),
            },
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(2),
            },
        ]);
        let requests = progress.service_reclamation_requests(2).unwrap();
        let deadlines = service_due(&mut progress, now, 2);
        assert_eq!(requests + deadlines, 4);
        assert_eq!(
            lock_unpoison(&bridge.reclaimed).as_slice(),
            &[OperationToken::decode(10), OperationToken::decode(11)]
        );
    }

    #[test]
    fn empty_io_inbox_transfers_entire_budget_to_due_deadlines() {
        let (mut progress, bridge) = progress(3);
        let now = Instant::now();
        for token in 1..=3 {
            progress.schedule_deadline_for_test(now, OperationToken::decode(token));
        }
        assert_eq!(service_due(&mut progress, now, 3), 3);
        assert_eq!(lock_unpoison(&bridge.reclaimed).len(), 3);
    }
}
