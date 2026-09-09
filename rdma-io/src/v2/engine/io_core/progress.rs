//! Bounded CQ, completion-dispatch, and operation-reclamation progress.

use std::collections::{HashSet, VecDeque};
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tokio::time::Instant;

use super::IoState;
use crate::v2::Completion;
use crate::v2::completion::CqReadiness;
use crate::v2::engine::config::CompletionMode;
use crate::v2::engine::progress::ReadinessRegistration;
use crate::v2::engine::registry::{ConnectionToken, OperationToken};
use crate::v2::engine::resources::EngineReactorResources;
use crate::v2::engine::scheduler::DeadlineQueue;
use crate::v2::error::{Error, Result};

/// I/O-owned progress state. It has no concrete dependency on session state.
pub(in crate::v2::engine) struct IoReactorSources {
    core: Option<IoState>,
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
        cq_budget: usize,
        completion_dispatch_budget: usize,
        reclamation_budget: usize,
        #[cfg(any(test, feature = "test-hooks"))] test_driver: Arc<
            crate::v2::engine::driver::test_api::TestDriverState,
        >,
    ) -> Self {
        Self {
            core: Some(core),
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
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use super::*;
    use crate::v2::engine::driver::test_api::TestDriverState;
    use crate::v2::engine::io_core::{
        EstablishedIoConnection, EstablishedIoIdentity, IoDeadlineRequest, IoDriverSignal, IoState,
        operation_future_for_io_lifetime_test,
    };

    struct NoopSignal;

    impl IoDriverSignal for NoopSignal {
        fn publish_cq_recheck(&self) {}
        fn publish_completion_dispatch(&self) {}
        fn publish_reclamation(&self) {}
        fn pause_operation_before_register(&self) {}
    }

    fn progress(reclamation_budget: usize) -> IoReactorSources {
        let signal: Arc<dyn IoDriverSignal> = Arc::new(NoopSignal);
        let core = IoState::new_owned(
            16,
            16,
            Duration::from_secs(1),
            3,
            Arc::new(RwLock::new(())),
            signal,
        )
        .unwrap();
        IoReactorSources::new(
            core,
            4,
            3,
            reclamation_budget,
            Arc::new(TestDriverState::new()),
        )
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
    fn operation_future_outlives_reactor_io_state() {
        let mut progress = progress(1);
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
        drop(progress);
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
        let mut progress = progress(1);
        progress.deadlines.exhaust_sequence_for_test();
        progress
            .core_mut()
            .reclamation_requests
            .push_back(IoDeadlineRequest {
                at: Instant::now(),
                token: OperationToken::decode(1),
            });
        let error = match progress.service_reclamation_requests(1) {
            Ok(_) => panic!("exhausted I/O deadline sequence must fail"),
            Err(error) => error,
        };

        assert!(matches!(error, Error::InvalidConfig(message) if message.contains("I/O deadline")));
    }
}
