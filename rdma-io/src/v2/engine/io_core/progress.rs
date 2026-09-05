//! Bounded CQ, completion-dispatch, and operation-reclamation progress.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet, VecDeque};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tokio::time::Instant;

use super::{IoCore, IoDeadlineRequest, IoSessionBridge};
use crate::v2::Completion;
use crate::v2::completion::CqReadiness;
use crate::v2::engine::config::CompletionMode;
use crate::v2::engine::progress::{ProgressReport, ReadinessRegistration};
use crate::v2::engine::registry::{ConnectionToken, OperationToken};
use crate::v2::engine::resources::IoProgressResources;
use crate::v2::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IoDeadlineEntry {
    at: Instant,
    sequence: u64,
    token: u64,
}

impl Ord for IoDeadlineEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

impl PartialOrd for IoDeadlineEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct IoDeadlineQueue {
    entries: BinaryHeap<Reverse<IoDeadlineEntry>>,
    next_sequence: u64,
}

impl IoDeadlineQueue {
    fn push(&mut self, request: IoDeadlineRequest) -> Result<()> {
        let sequence = self.next_sequence;
        self.next_sequence = sequence.checked_add(1).ok_or_else(|| {
            Error::InvalidConfig("I/O deadline insertion sequence exhausted".into())
        })?;
        self.entries.push(Reverse(IoDeadlineEntry {
            at: request.at,
            sequence,
            token: request.token.encode(),
        }));
        Ok(())
    }

    fn pop_one_due(&mut self, now: Instant) -> Option<OperationToken> {
        let Reverse(entry) = self.entries.peek().copied()?;
        if entry.at > now {
            return None;
        }
        self.entries.pop();
        Some(OperationToken::decode(entry.token))
    }

    fn next(&self) -> Option<Instant> {
        self.entries.peek().map(|entry| entry.0.at)
    }
}

/// I/O-owned progress state. It has no concrete dependency on session state.
pub(in crate::v2::engine) struct IoProgress {
    core: Arc<IoCore>,
    resources: Option<IoProgressResources>,
    cq_readiness: CqReadiness,
    cq_buffer: Box<[Completion]>,
    completion_connections: CompletionConnections,
    deadlines: IoDeadlineQueue,
    reclamation_turn_starts_with_request: bool,
    completion_dispatch_budget: usize,
    reclamation_budget: usize,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: Arc<crate::v2::engine::driver::test_api::TestDriverState>,
}

impl IoProgress {
    pub(in crate::v2::engine) fn new(
        core: Arc<IoCore>,
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
            resources,
            cq_readiness: CqReadiness::default(),
            cq_buffer: vec![Completion::default(); cq_budget].into_boxed_slice(),
            completion_connections: CompletionConnections::default(),
            deadlines: IoDeadlineQueue::default(),
            reclamation_turn_starts_with_request: true,
            completion_dispatch_budget,
            reclamation_budget,
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver,
        }
    }

    pub(in crate::v2::engine) fn turn(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> Result<ProgressReport> {
        let (cq_units, readiness, cq_repoll) = self.service_cq(mode, cx)?;
        let (reclamation_units, reclamation_ready) = self.service_reclamation()?;
        let (dispatch_units, dispatch_ready) = self.service_completion_dispatch()?;
        let units_consumed = cq_units
            .saturating_add(reclamation_units)
            .saturating_add(dispatch_units);
        Ok(ProgressReport::running(
            units_consumed,
            cq_repoll || reclamation_ready || dispatch_ready,
            self.deadlines.next(),
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

    #[cfg(test)]
    pub(in crate::v2::engine) fn completion_connection_count(&self) -> usize {
        self.completion_connections.len()
    }

    #[cfg(test)]
    fn cq_buffer_capacity(&self) -> usize {
        self.cq_buffer.len()
    }

    fn bridge(&self) -> Result<Arc<dyn IoSessionBridge>> {
        self.core.session_bridge().ok_or(Error::DriverShutdown)
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
            if let Some(connection) = self.bridge()?.route_completion(completion) {
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
        let bridge = self.bridge()?;
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
        let bridge = self.bridge()?;
        let now = Instant::now();
        let mut consumed = 0;
        let starts_with_request = self.reclamation_turn_starts_with_request;
        self.reclamation_turn_starts_with_request = !starts_with_request;
        let mut prefer_request = starts_with_request;
        while consumed < self.reclamation_budget {
            let handled = if prefer_request {
                self.ingest_one_request()? || self.process_one_deadline(now, bridge.as_ref())
            } else {
                self.process_one_deadline(now, bridge.as_ref()) || self.ingest_one_request()?
            };
            if !handled {
                break;
            }
            consumed += 1;
            prefer_request = !prefer_request;
        }
        let immediate = self.core.has_reclamation_requests()
            || self.deadlines.next().is_some_and(|at| at <= now);
        Ok((consumed, immediate))
    }

    fn ingest_one_request(&mut self) -> Result<bool> {
        let Some(request) = self.core.take_reclamation_requests(1).into_iter().next() else {
            return Ok(false);
        };
        self.deadlines.push(request)?;
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
            .bridge()?
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
            .push(IoDeadlineRequest { at, token })
            .expect("test I/O deadline insertion");
    }

    #[cfg(test)]
    fn reclamation_turn_starts_with_request(&self) -> bool {
        self.reclamation_turn_starts_with_request
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
    use crate::v2::engine::io_core::IoDriverSignal;
    use crate::v2::engine::progress::{EffectsPublication, ProgressTerminal};
    use crate::v2::engine::registry::lock_unpoison;

    struct NoopSignal;

    impl IoDriverSignal for NoopSignal {
        fn publish_cq_recheck(&self) {}
        fn publish_completion_dispatch(&self) {}
        fn publish_reclamation(&self) {}
        fn publish_terminal(&self) {}
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
        core.bind_session_bridge(&bridge_dyn);
        let progress = IoProgress::new(
            Arc::clone(&core),
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
    fn operation_deadlines_are_ordered_and_budgetable() {
        let now = Instant::now();
        let mut deadlines = IoDeadlineQueue::default();
        deadlines
            .push(IoDeadlineRequest {
                at: now + Duration::from_secs(2),
                token: OperationToken::decode(2),
            })
            .unwrap();
        deadlines
            .push(IoDeadlineRequest {
                at: now,
                token: OperationToken::decode(0),
            })
            .unwrap();
        deadlines
            .push(IoDeadlineRequest {
                at: now + Duration::from_secs(1),
                token: OperationToken::decode(1),
            })
            .unwrap();

        assert_eq!(
            deadlines.pop_one_due(now + Duration::from_secs(1)),
            Some(OperationToken::decode(0))
        );
        assert_eq!(
            deadlines.pop_one_due(now + Duration::from_secs(1)),
            Some(OperationToken::decode(1))
        );
        assert_eq!(deadlines.next(), Some(now + Duration::from_secs(2)));
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
        assert!(matches!(report.terminal, ProgressTerminal::Running));
        assert_eq!(report.effects, EffectsPublication::Complete);
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
}
