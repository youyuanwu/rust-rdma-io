//! Bounded CM, lifecycle-deadline, and shutdown progress.

use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tokio::time::Instant;

use super::SessionManager;
use super::cm::CmShutdownCursor;
use crate::v2::engine::config::CompletionMode;
use crate::v2::engine::lifecycle::MemoizedTerminalResult;
use crate::v2::engine::progress::{
    EffectsPublication, ProgressReport, ProgressTerminal, ReadinessRegistration,
};
use crate::v2::engine::resources::SessionProgressResources;
use crate::v2::engine::scheduler::{Deadline, DeadlineKind, DeadlineQueue};
use crate::v2::error::{Error, Result};

pub(in crate::v2::engine) struct SessionProgress {
    manager: Arc<SessionManager>,
    resources: Option<SessionProgressResources>,
    deadlines: DeadlineQueue,
    reclamation_turn_starts_with_request: bool,
    cm_next_source: usize,
    shutdown_started: bool,
    shutdown_cm: CmShutdownCursor,
    shutdown_connection_slot: usize,
    shutdown_connections_complete: bool,
    shutdown_next_source: bool,
    terminal_completion_ready: bool,
    cm_budget: usize,
    reclamation_budget: usize,
}

impl SessionProgress {
    pub(in crate::v2::engine) fn new(
        manager: Arc<SessionManager>,
        resources: Option<SessionProgressResources>,
        cm_budget: usize,
        reclamation_budget: usize,
    ) -> Self {
        Self {
            manager,
            resources,
            deadlines: DeadlineQueue::default(),
            reclamation_turn_starts_with_request: true,
            cm_next_source: 0,
            shutdown_started: false,
            shutdown_cm: CmShutdownCursor::default(),
            shutdown_connection_slot: 0,
            shutdown_connections_complete: false,
            shutdown_next_source: true,
            terminal_completion_ready: false,
            cm_budget,
            reclamation_budget,
        }
    }

    pub(in crate::v2::engine) fn turn(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> Result<ProgressReport> {
        let Some(shared) = self.manager.engine() else {
            return Err(Error::DriverShutdown);
        };
        let shutting_down = shared
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire);
        if shutting_down {
            self.terminal_completion_ready = false;
            self.ensure_shutdown_started();
        }
        let (cm_units, readiness, cm_ready, observed_would_block) =
            self.service_cm(mode, cx, &shared, shutting_down)?;
        let (deadline_units, deadline_ready, deadline_terminal) =
            self.service_deadlines(&shared)?;
        if shutting_down
            && observed_would_block
            && self.shutdown_issuance_complete()
            && self.terminal_state_drained(&shared)
        {
            #[cfg(any(test, feature = "test-hooks"))]
            if let Some(resources) = self.resources.as_ref() {
                crate::test_support::destruction::record(
                    crate::test_support::destruction::DestructionKind::CmFinalDrainToWouldBlock,
                    resources.engine().cm_event_channel.as_raw() as usize,
                );
            }
            self.terminal_completion_ready = true;
        }
        let terminal = if deadline_terminal || self.terminal_completion_ready {
            ProgressTerminal::Ready
        } else {
            ProgressTerminal::Running
        };
        Ok(ProgressReport {
            units_consumed: cm_units.saturating_add(deadline_units),
            immediate_work: cm_ready || deadline_ready,
            next_deadline: self.deadlines.next(),
            readiness,
            terminal,
            effects: EffectsPublication::Complete,
        })
    }

    pub(in crate::v2::engine) fn can_finish(&self) -> bool {
        self.terminal_completion_ready
            && self.shutdown_issuance_complete()
            && self
                .manager
                .engine()
                .is_some_and(|shared| self.terminal_state_drained(&shared))
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
    fn reclamation_turn_starts_with_request(&self) -> bool {
        self.reclamation_turn_starts_with_request
    }

    fn ensure_shutdown_started(&mut self) {
        if self.shutdown_started {
            return;
        }
        self.shutdown_started = true;
        self.manager.cm.start_bounded_shutdown();
        self.manager
            .shutdown_connection_close_started
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn shutdown_issuance_complete(&self) -> bool {
        self.shutdown_started
            && self.shutdown_connections_complete
            && self.manager.cm.bounded_shutdown_complete(&self.shutdown_cm)
    }

    fn terminal_state_drained(&self, shared: &Arc<crate::v2::engine::EngineShared>) -> bool {
        !self.manager.has_cm_work()
            && self.manager.retained_cm_owner_count() == 0
            && self.manager.live_connection_count() == 0
            && shared.io_core.accepted_count() == 0
    }

    fn service_shutdown_unit(&mut self, shared: &Arc<crate::v2::engine::EngineShared>) -> usize {
        let outcome = MemoizedTerminalResult::from_error(Error::DriverShutdown);
        for _ in 0..2 {
            let cm_first = self.shutdown_next_source;
            self.shutdown_next_source = !self.shutdown_next_source;
            if cm_first {
                let processed = self.manager.cm.service_bounded_shutdown(
                    shared,
                    &outcome,
                    &mut self.shutdown_cm,
                    1,
                );
                if processed != 0 {
                    return processed;
                }
            } else if !self.shutdown_connections_complete {
                let (connections, next, complete, scanned) = self
                    .manager
                    .connections
                    .scan_occupied(self.shutdown_connection_slot, 1);
                self.shutdown_connection_slot = next;
                self.shutdown_connections_complete = complete;
                for connection in connections {
                    self.manager.begin_connection_close(shared, &connection);
                }
                if scanned != 0 {
                    return scanned;
                }
            }
        }
        0
    }

    fn service_cm(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
        shared: &Arc<crate::v2::engine::EngineShared>,
        shutting_down: bool,
    ) -> Result<(usize, ReadinessRegistration, bool, bool)> {
        let mut processed = 0;
        let mut readiness = if mode == CompletionMode::Readiness && self.resources.is_some() {
            ReadinessRegistration::Incomplete
        } else {
            ReadinessRegistration::NotRequired
        };
        let mut readiness_checked = false;
        let mut observed_would_block = self.resources.is_none();
        while processed < self.cm_budget {
            let mut selected = false;
            for offset in 0..4 {
                let source = (self.cm_next_source + offset) % 4;
                let units = match source {
                    0 => self.manager.service_cm_software(
                        shared,
                        self.resources
                            .as_ref()
                            .map(SessionProgressResources::engine),
                        1,
                    )?,
                    1 => {
                        let Some(resources) = self.resources.as_ref() else {
                            continue;
                        };
                        let resources = resources.engine();
                        if self.manager.try_process_cm_event(shared, resources)? {
                            observed_would_block = false;
                            readiness = if mode == CompletionMode::Readiness {
                                ReadinessRegistration::Incomplete
                            } else {
                                ReadinessRegistration::NotRequired
                            };
                            1
                        } else {
                            observed_would_block = true;
                            if mode == CompletionMode::Readiness && !readiness_checked {
                                readiness_checked = true;
                                let async_fd = resources.cm_async_fd.as_ref().ok_or_else(|| {
                                    Error::InvalidConfig(
                                        "readiness engine has no CM AsyncFd".into(),
                                    )
                                })?;
                                match poll_readiness_events(
                                    cx,
                                    1,
                                    |cx| match async_fd.poll_read_ready(cx) {
                                        Poll::Ready(Ok(guard)) => Poll::Ready(Ok(guard)),
                                        Poll::Ready(Err(error)) => {
                                            Poll::Ready(Err(Error::Verbs(error)))
                                        }
                                        Poll::Pending => Poll::Pending,
                                    },
                                    |guard| guard.clear_ready(),
                                    || self.manager.try_process_cm_event(shared, resources),
                                ) {
                                    Poll::Ready(result) => {
                                        let units = result?;
                                        if units != 0 {
                                            observed_would_block = false;
                                            readiness = ReadinessRegistration::Incomplete;
                                        }
                                        units
                                    }
                                    Poll::Pending => {
                                        readiness = ReadinessRegistration::RegisteredAndRechecked;
                                        0
                                    }
                                }
                            } else {
                                0
                            }
                        }
                    }
                    2 => {
                        let Some(resources) = self.resources.as_ref() else {
                            continue;
                        };
                        let resources = resources.engine();
                        self.manager
                            .service_deferred_cm_destructions(shared, 1, || {
                                self.manager.try_process_cm_event(shared, resources)
                            })?
                    }
                    3 if shutting_down && !self.shutdown_issuance_complete() => {
                        self.service_shutdown_unit(shared)
                    }
                    _ => 0,
                };
                if units == 0 {
                    continue;
                }
                processed += units;
                self.cm_next_source = (source + 1) % 4;
                selected = true;
                break;
            }
            if !selected {
                break;
            }
        }

        let immediate = processed >= self.cm_budget
            || self.manager.has_cm_work()
            || (shutting_down && !self.shutdown_issuance_complete());
        Ok((processed, readiness, immediate, observed_would_block))
    }

    fn service_deadlines(
        &mut self,
        shared: &Arc<crate::v2::engine::EngineShared>,
    ) -> Result<(usize, bool, bool)> {
        let now = Instant::now();
        let starts_with_request = self.reclamation_turn_starts_with_request;
        self.reclamation_turn_starts_with_request = !starts_with_request;
        let mut prefer_request = starts_with_request;
        let mut consumed = 0;
        let mut terminal_ready = false;
        while consumed < self.reclamation_budget {
            let handled = if prefer_request {
                self.ingest_one_deadline()?
                    || self.process_one_deadline(now, shared, &mut terminal_ready)?
            } else {
                self.process_one_deadline(now, shared, &mut terminal_ready)?
                    || self.ingest_one_deadline()?
            };
            if !handled {
                break;
            }
            consumed += 1;
            prefer_request = !prefer_request;
        }
        let immediate = self.manager.has_deadline_requests()
            || self.deadlines.next().is_some_and(|at| at <= now);
        Ok((consumed, immediate, terminal_ready))
    }

    fn ingest_one_deadline(&mut self) -> Result<bool> {
        let Some(request) = self.manager.take_deadline_requests(1).into_iter().next() else {
            return Ok(false);
        };
        if !self.deadlines.push(request.at, request.kind, request.token) {
            return Err(Error::InvalidConfig(
                "session deadline insertion sequence exhausted".into(),
            ));
        }
        Ok(true)
    }

    fn process_one_deadline(
        &mut self,
        now: Instant,
        shared: &Arc<crate::v2::engine::EngineShared>,
        terminal_ready: &mut bool,
    ) -> Result<bool> {
        let Some(deadline) = self.deadlines.pop_due(now, 1).into_iter().next() else {
            return Ok(false);
        };
        match deadline {
            Deadline {
                kind: DeadlineKind::EngineShutdown,
                ..
            } => {
                if let Some(failure) = shared.shutdown_deadline_failure() {
                    return Err(failure);
                }
                *terminal_ready = true;
            }
            Deadline {
                kind: DeadlineKind::ConnectionDrain,
                token,
                ..
            } => self.manager.handle_connection_drain_deadline(
                shared,
                crate::v2::engine::registry::ConnectionToken::decode(token),
            ),
        }
        Ok(true)
    }
}

fn poll_readiness_events<G>(
    cx: &mut TaskContext<'_>,
    budget: usize,
    mut poll_read_ready: impl FnMut(&mut TaskContext<'_>) -> Poll<Result<G>>,
    mut clear_ready: impl FnMut(&mut G),
    mut try_one: impl FnMut() -> Result<bool>,
) -> Poll<Result<usize>> {
    debug_assert!(budget > 0);
    let mut processed = 0;
    loop {
        let mut guard = match poll_read_ready(cx) {
            Poll::Ready(Ok(guard)) => guard,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending if processed == 0 => return Poll::Pending,
            Poll::Pending => return Poll::Ready(Ok(processed)),
        };
        loop {
            if processed == budget {
                return Poll::Ready(Ok(processed));
            }
            match try_one() {
                Ok(true) => processed += 1,
                Ok(false) => {
                    clear_ready(&mut guard);
                    break;
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::task::Waker;

    use super::*;
    use crate::v2::engine::scheduler::DeadlineKind;
    use crate::v2::engine::test_engine_pair;

    #[test]
    fn cm_event_arriving_during_clear_is_drained_after_reregister() {
        #[derive(Default)]
        struct FakeReadiness {
            ready: bool,
            event_available: bool,
            polls: usize,
            clears: usize,
        }

        let state = Rc::new(RefCell::new(FakeReadiness {
            ready: true,
            ..FakeReadiness::default()
        }));
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let result = poll_readiness_events(
            &mut cx,
            8,
            {
                let state = Rc::clone(&state);
                move |_| {
                    let mut state = state.borrow_mut();
                    state.polls += 1;
                    if state.ready {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Pending
                    }
                }
            },
            {
                let state = Rc::clone(&state);
                move |_| {
                    let mut state = state.borrow_mut();
                    state.clears += 1;
                    if state.clears == 1 {
                        state.event_available = true;
                        state.ready = true;
                    } else {
                        state.ready = false;
                    }
                }
            },
            {
                let state = Rc::clone(&state);
                move || {
                    let mut state = state.borrow_mut();
                    if state.event_available {
                        state.event_available = false;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(1))));
        let state = state.borrow();
        assert_eq!(state.polls, 3);
        assert_eq!(state.clears, 2);
        assert!(!state.event_available);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_turn_start_alternates_with_an_odd_budget() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let mut progress = SessionProgress::new(Arc::clone(&engine.shared.session), None, 1, 1);
        engine.shared.session.schedule_deadline(
            &engine.shared.work_signal,
            DeadlineKind::ConnectionDrain,
            1,
            std::time::Duration::ZERO,
        );
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        assert!(progress.reclamation_turn_starts_with_request());
        let first = progress.turn(CompletionMode::Polling, &mut cx).unwrap();
        assert_eq!(first.units_consumed, 1);
        assert!(first.immediate_work);
        assert!(!progress.reclamation_turn_starts_with_request());

        let second = progress.turn(CompletionMode::Polling, &mut cx).unwrap();
        assert_eq!(second.units_consumed, 1);
        assert!(progress.reclamation_turn_starts_with_request());

        drop(driver);
    }

    #[tokio::test]
    async fn shutdown_scan_and_final_drain_are_bounded_session_work() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connections = engine
            .shared
            .test_driver
            .install_idle_connections(&engine.shared, 64)
            .unwrap();
        engine.shared.request_shutdown();
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let first = driver
            .session_progress
            .turn(CompletionMode::Polling, &mut cx)
            .unwrap();
        let closed = connections
            .iter()
            .filter(|connection| connection.state.close_started())
            .count();
        assert!(closed > 0 && closed < connections.len());
        assert!(first.units_consumed <= 48);
        assert!(first.immediate_work);
        assert!(!matches!(first.terminal, ProgressTerminal::Ready));

        drop(connections);
        drop(driver);
    }

    #[tokio::test]
    async fn idle_shutdown_reports_terminal_only_from_the_bounded_turn() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        engine.shared.request_shutdown();
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let mut ready = false;
        for _ in 0..4 {
            let report = driver
                .session_progress
                .turn(CompletionMode::Polling, &mut cx)
                .unwrap();
            assert!(report.units_consumed <= 48);
            if matches!(report.terminal, ProgressTerminal::Ready) {
                ready = true;
                break;
            }
        }
        assert!(ready);
        assert!(driver.session_progress.can_finish());

        engine.shared.finish(MemoizedTerminalResult::success());
        drop(driver);
    }
}
