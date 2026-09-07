//! Bounded CQ, completion-dispatch, and operation-reclamation progress.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tokio::time::Instant;

#[cfg(test)]
use super::IoDeadlineRequest;
use super::{IoCore, IoSessionBridge};
use crate::v2::Completion;
use crate::v2::completion::CqReadiness;
use crate::v2::engine::config::CompletionMode;
use crate::v2::engine::progress::{ProgressReport, ReadinessRegistration};
use crate::v2::engine::registry::{ConnectionToken, OperationToken};
use crate::v2::engine::resources::IoProgressResources;
use crate::v2::engine::scheduler::{AlternatingSources, DeadlineQueue, Source};
use crate::v2::error::{Error, Result};

/// I/O-owned progress state. It has no concrete dependency on session state.
pub(in crate::v2::engine) struct IoProgress {
    core: Arc<IoCore>,
    bridge: Arc<dyn IoSessionBridge>,
    resources: Option<IoProgressResources>,
    cq_readiness: CqReadiness,
    cq_buffer: Box<[Completion]>,
    completion_connections: CompletionConnections,
    deadlines: DeadlineQueue<OperationToken>,
    reclamation_sources: AlternatingSources,
    completion_dispatch_budget: usize,
    reclamation_budget: usize,
    terminal_cursor: usize,
    terminal_complete: bool,
    #[cfg(test)]
    turns: usize,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: Arc<crate::v2::engine::driver::test_api::TestDriverState>,
}

impl IoProgress {
    pub(in crate::v2::engine) fn new(
        core: Arc<IoCore>,
        bridge: Arc<dyn IoSessionBridge>,
        resources: Option<IoProgressResources>,
        cq_budget: usize,
        completion_dispatch_budget: usize,
        reclamation_budget: usize,
        #[cfg(any(test, feature = "test-hooks"))] test_driver: Arc<
            crate::v2::engine::driver::test_api::TestDriverState,
        >,
    ) -> Self {
        Self {
            core,
            bridge,
            resources,
            cq_readiness: CqReadiness::default(),
            cq_buffer: vec![Completion::default(); cq_budget].into_boxed_slice(),
            completion_connections: CompletionConnections::default(),
            deadlines: DeadlineQueue::default(),
            reclamation_sources: AlternatingSources::default(),
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

    pub(in crate::v2::engine) fn turn(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> Result<ProgressReport> {
        #[cfg(test)]
        {
            self.turns = self.turns.saturating_add(1);
        }
        if let Some(outcome) = self.core.terminal_failure() {
            let budget = self
                .cq_buffer
                .len()
                .saturating_add(self.completion_dispatch_budget)
                .saturating_add(self.reclamation_budget);
            let (effects, next, complete, scanned) =
                self.core
                    .terminalize_operations_bounded(&outcome, self.terminal_cursor, budget);
            self.terminal_cursor = next;
            self.terminal_complete = complete;
            self.bridge.commit_terminal_effects(effects);
            return Ok(ProgressReport::running(
                scanned,
                !complete,
                ReadinessRegistration::NotRequired,
            ));
        }
        let (cq_units, readiness, cq_repoll) = self.service_cq(mode, cx)?;
        let (reclamation_units, reclamation_ready) = self.service_reclamation()?;
        let (dispatch_units, dispatch_ready) = self.service_completion_dispatch()?;
        let units_consumed = cq_units
            .saturating_add(reclamation_units)
            .saturating_add(dispatch_units);
        Ok(ProgressReport::running(
            units_consumed,
            cq_repoll || reclamation_ready || dispatch_ready,
            readiness,
        ))
    }

    pub(in crate::v2::engine) fn release_resources(&mut self) {
        if let Some(resources) = self.resources.as_mut() {
            resources.drop_readiness_adapter();
        }
        self.resources.take();
    }

    pub(in crate::v2::engine) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.next()
    }

    pub(in crate::v2::engine) fn can_finish(&self) -> bool {
        if self.core.terminal_failure().is_some() {
            return self.terminal_complete;
        }
        self.core.shutdown_requested() && self.core.accepted_count() == 0
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn completion_connection_count(&self) -> usize {
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

    fn bridge(&self) -> Arc<dyn IoSessionBridge> {
        Arc::clone(&self.bridge)
    }

    fn enqueue_connection(&mut self, connection: ConnectionToken) {
        self.completion_connections.enqueue(connection);
    }

    fn service_cq(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> Result<(usize, ReadinessRegistration, bool)> {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(completion) = self.test_driver.take_released_connection_cqe() {
            if let Some(connection) = self.bridge.route_completion(completion) {
                self.enqueue_connection(connection);
            }
            return Ok((1, ReadinessRegistration::Incomplete, true));
        }

        let Some(resources) = self.resources.as_ref() else {
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
        let bridge = self.bridge();
        let completions = self.cq_buffer[..count].to_vec();
        for completion in completions {
            let completion = completion.into_raw();
            #[cfg(any(test, feature = "test-hooks"))]
            if self.test_driver.suppress_connection_cqe(completion) {
                continue;
            }
            if let Some(connection) = bridge.route_completion(completion) {
                self.enqueue_connection(connection);
            }
            #[cfg(any(test, feature = "test-hooks"))]
            self.test_driver.dispatch(completion);
            #[cfg(not(any(test, feature = "test-hooks")))]
            let _ = completion;
        }
        Ok((count, readiness, true))
    }

    fn service_reclamation(&mut self) -> Result<(usize, bool)> {
        let bridge = self.bridge();
        let now = Instant::now();
        let mut consumed = 0;
        let mut sources = self.reclamation_sources.begin_turn();
        while consumed < self.reclamation_budget {
            let mut handled = false;
            for source in sources.order() {
                handled = match source {
                    Source::First => self.ingest_one_request()?,
                    Source::Second => self.process_one_deadline(now, bridge.as_ref()),
                };
                if handled {
                    break;
                }
            }
            if !handled {
                break;
            }
            consumed += 1;
            sources.consumed();
        }
        let immediate = self.core.has_reclamation_requests()
            || self.deadlines.next().is_some_and(|at| at <= now);
        Ok((consumed, immediate))
    }

    fn ingest_one_request(&mut self) -> Result<bool> {
        let Some(request) = self.core.take_reclamation_requests(1).into_iter().next() else {
            return Ok(false);
        };
        self.deadlines
            .push(request.at, request.token)
            .map_err(|_| {
                Error::InvalidConfig("I/O deadline insertion sequence exhausted".into())
            })?;
        Ok(true)
    }

    fn process_one_deadline(&mut self, now: Instant, bridge: &dyn IoSessionBridge) -> bool {
        let Some(token) = self.deadlines.pop_one_due(now) else {
            return false;
        };
        bridge.handle_reclamation_deadline(token);
        true
    }

    fn service_completion_dispatch(&mut self) -> Result<(usize, bool)> {
        if let Some(connection) = self.core.take_published_connection() {
            self.enqueue_connection(connection);
        }
        let Some(connection) = self.completion_connections.pop() else {
            return Ok((0, self.core.has_published_connections()));
        };
        let (processed, remains_ready) = self
            .bridge
            .dispatch_connection_completions(connection, self.completion_dispatch_budget);
        if remains_ready {
            self.enqueue_connection(connection);
        }
        Ok((
            processed,
            self.completion_connections.len() > 0 || self.core.has_published_connections(),
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
    fn reclamation_turn_starts_with_request(&self) -> bool {
        self.reclamation_sources.first_starts_next_turn()
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
        EstablishedIoConnection, EstablishedIoIdentity, IoDriverSignal, IoPostAuthority,
        operation_future_for_io_lifetime_test,
    };
    use crate::v2::engine::registry::lock_unpoison;
    use crate::v2::qp::BatchPostOutcome;
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct NoopSignal;

    impl IoDriverSignal for NoopSignal {
        fn publish_cq_recheck(&self) {}
        fn publish_completion_dispatch(&self) {}
        fn publish_reclamation(&self) {}
        fn pause_operation_before_register(&self) {}
    }

    struct NoopPoster;

    impl IoPostAuthority for NoopPoster {
        fn qp_num(&self) -> u32 {
            7
        }

        fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            unreachable!("lifetime-only operation is already in flight")
        }

        fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            unreachable!("lifetime-only operation is already in flight")
        }
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
            _completion: crate::wc::WorkCompletion,
        ) -> Option<ConnectionToken> {
            None
        }

        fn dispatch_connection_completions(
            &self,
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

        fn handle_reclamation_deadline(&self, token: OperationToken) {
            lock_unpoison(&self.reclaimed).push(token);
        }

        fn commit_terminal_effects(&self, _effects: super::super::IoCoreEffects) {}
    }

    fn progress(reclamation_budget: usize) -> (IoProgress, Arc<IoCore>, Arc<RecordingBridge>) {
        let signal: Arc<dyn IoDriverSignal> = Arc::new(NoopSignal);
        let (core, _) = IoCore::new(
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
        let progress = IoProgress::new(
            Arc::clone(&core),
            bridge_dyn,
            None,
            4,
            3,
            reclamation_budget,
            Arc::new(TestDriverState::new()),
        );
        (progress, core, bridge)
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
        let (progress, core, bridge) = progress(1);
        let bridge_weak = Arc::downgrade(&bridge);
        let connection = EstablishedIoConnection::new(
            EstablishedIoIdentity {
                connection: connection(7),
                qp_num: 7,
            },
            Arc::new(NoopPoster),
            1,
            1,
            Arc::new(tokio::sync::Notify::new()),
        );
        let operation = operation_future_for_io_lifetime_test(&core, &connection);

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
        let (mut progress, core, _bridge) = progress(1);
        progress.deadlines.exhaust_sequence_for_test();
        lock_unpoison(&core.reclamation_requests).push_back(IoDeadlineRequest {
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
        let (mut progress, _core, bridge) = progress(1);
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
    fn odd_reclamation_budget_alternates_the_starting_source_between_turns() {
        let (mut progress, core, bridge) = progress(1);
        let now = Instant::now();
        lock_unpoison(&core.reclamation_requests).extend([
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(1),
            },
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(2),
            },
        ]);
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);

        assert!(progress.reclamation_turn_starts_with_request());
        let first = progress.turn(CompletionMode::Polling, &mut cx).unwrap();
        assert_eq!(first.units_consumed, 1);
        assert!(first.immediate_work);
        assert!(!progress.reclamation_turn_starts_with_request());
        assert!(lock_unpoison(&bridge.reclaimed).is_empty());

        let second = progress.turn(CompletionMode::Polling, &mut cx).unwrap();
        assert_eq!(second.units_consumed, 1);
        assert_eq!(
            lock_unpoison(&bridge.reclaimed).as_slice(),
            &[OperationToken::decode(1)]
        );
        assert!(progress.reclamation_turn_starts_with_request());
    }

    #[test]
    fn even_reclamation_budget_still_flips_the_next_turn_preference() {
        let (mut progress, core, _bridge) = progress(2);
        let now = Instant::now();
        lock_unpoison(&core.reclamation_requests).push_back(IoDeadlineRequest {
            at: now,
            token: OperationToken::decode(1),
        });
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);

        let report = progress.turn(CompletionMode::Polling, &mut cx).unwrap();

        assert_eq!(report.units_consumed, 2);
        assert!(!progress.reclamation_turn_starts_with_request());
    }

    #[test]
    fn sustained_io_sources_alternate_and_leave_bounded_work_for_next_turn() {
        let (mut progress, core, bridge) = progress(4);
        let now = Instant::now();
        progress.schedule_deadline_for_test(now, OperationToken::decode(10));
        progress.schedule_deadline_for_test(now, OperationToken::decode(11));
        lock_unpoison(&core.reclamation_requests).extend([
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(1),
            },
            IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(2),
            },
        ]);
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);

        let report = progress.turn(CompletionMode::Polling, &mut cx).unwrap();

        assert_eq!(report.units_consumed, 4);
        assert!(report.immediate_work);
        assert_eq!(
            lock_unpoison(&bridge.reclaimed).as_slice(),
            &[OperationToken::decode(10), OperationToken::decode(11)]
        );
    }

    #[test]
    fn empty_io_inbox_transfers_entire_budget_to_due_deadlines() {
        let (mut progress, _core, bridge) = progress(3);
        let now = Instant::now();
        for token in 1..=3 {
            progress.schedule_deadline_for_test(now, OperationToken::decode(token));
        }
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);

        let report = progress.turn(CompletionMode::Polling, &mut cx).unwrap();

        assert_eq!(report.units_consumed, 3);
        assert!(!report.immediate_work);
        assert_eq!(lock_unpoison(&bridge.reclaimed).len(), 3);
    }
}
