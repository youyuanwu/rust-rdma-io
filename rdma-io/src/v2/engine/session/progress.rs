//! Bounded CM, lifecycle-deadline, and shutdown progress.

use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tokio::time::Instant;

use super::cm::CmShutdownCursor;
use super::{
    CmShutdownClass, CmShutdownSnapshot, CmSoftwareClass, CmSoftwareSnapshot, DeadlineKind,
    SessionManager,
};
use crate::v2::engine::config::CompletionMode;
use crate::v2::engine::lifecycle::MemoizedTerminalResult;
#[cfg(test)]
use crate::v2::engine::progress::ProgressReport;
use crate::v2::engine::progress::ReadinessRegistration;
use crate::v2::engine::resources::SessionProgressResources;
use crate::v2::engine::scheduler::DeadlineQueue;
use crate::v2::error::{Error, Result};

pub(in crate::v2::engine) struct SessionReactorSources {
    manager: Arc<SessionManager>,
    resources: Option<SessionProgressResources>,
    deadlines: DeadlineQueue<SessionDeadline>,
    ready_deadlines: std::collections::VecDeque<SessionDeadline>,
    shutdown_started: bool,
    shutdown_cm: CmShutdownCursor,
    shutdown_connection_slot: usize,
    shutdown_connections_complete: bool,
    failure_scan_started: bool,
    terminal_completion_ready: bool,
    #[cfg(test)]
    turns: usize,
    cm_budget: usize,
    reclamation_budget: usize,
}

impl SessionReactorSources {
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
            ready_deadlines: std::collections::VecDeque::new(),
            shutdown_started: false,
            shutdown_cm: CmShutdownCursor::default(),
            shutdown_connection_slot: 0,
            shutdown_connections_complete: false,
            failure_scan_started: false,
            terminal_completion_ready: false,
            #[cfg(test)]
            turns: 0,
            cm_budget,
            reclamation_budget,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn turn(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> Result<ProgressReport> {
        let (shutting_down, terminal_failure) = self.begin_turn()?;
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let mut cm_units = 0;
        if !terminal_failure {
            let snapshot = self.cm_software_snapshot();
            for class in CmSoftwareClass::ALL {
                cm_units += self.service_cm_software_class(
                    class,
                    snapshot
                        .count(class)
                        .min(self.cm_budget.saturating_sub(cm_units)),
                    &mut actions,
                )?;
                if cm_units == self.cm_budget {
                    break;
                }
            }
        }
        let (events, readiness, observed_would_block) = self.service_cm_events(
            mode,
            cx,
            self.cm_budget.saturating_sub(cm_units),
            &mut actions,
        )?;
        cm_units += events;
        if !terminal_failure {
            cm_units += self.service_cm_destructions(
                self.cm_budget.saturating_sub(cm_units),
                observed_would_block,
                &mut actions,
            )?;
        }
        if shutting_down {
            let snapshot = self.shutdown_snapshot();
            for class in CmShutdownClass::ALL {
                cm_units += self.service_cm_shutdown_class(
                    class,
                    snapshot
                        .count(class)
                        .min(self.cm_budget.saturating_sub(cm_units)),
                    &mut actions,
                );
                if cm_units == self.cm_budget {
                    break;
                }
            }
            if cm_units < self.cm_budget && self.shutdown_connections_ready() {
                cm_units +=
                    self.service_shutdown_connections(self.cm_budget - cm_units, &mut actions);
            }
        }
        let (deadline_units, deadline_ready) = if terminal_failure {
            (0, false)
        } else {
            self.service_deadlines_for_test()?
        };
        self.finish_turn(shutting_down, terminal_failure, observed_would_block);
        actions.publish();
        let cm_ready = cm_units >= self.cm_budget
            || self.manager.has_cm_work()
            || (shutting_down && !self.shutdown_issuance_complete());
        Ok(ProgressReport::running(
            cm_units.saturating_add(deadline_units),
            cm_ready || deadline_ready,
            readiness,
        ))
    }

    pub(in crate::v2::engine) fn begin_turn(&mut self) -> Result<(bool, bool)> {
        #[cfg(test)]
        {
            self.turns = self.turns.saturating_add(1);
        }
        if self.manager.engine_runtime().is_none() {
            return Err(Error::DriverShutdown);
        }
        let shutting_down = self.manager.shutdown_requested();
        let terminal_failure = self.manager.pending_terminal_outcome().is_some();
        if shutting_down {
            self.terminal_completion_ready = false;
            self.ensure_shutdown_started();
        }
        if terminal_failure {
            self.prepare_failure_scan();
        }
        Ok((shutting_down, terminal_failure))
    }

    pub(in crate::v2::engine) fn finish_turn(
        &mut self,
        shutting_down: bool,
        terminal_failure: bool,
        observed_would_block: bool,
    ) {
        if terminal_failure && self.shutdown_issuance_complete() {
            self.terminal_completion_ready = true;
        } else if shutting_down
            && observed_would_block
            && self.shutdown_issuance_complete()
            && self.terminal_state_drained()
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
    }

    pub(in crate::v2::engine) fn cm_budget(&self) -> usize {
        self.cm_budget
    }

    pub(in crate::v2::engine) fn resources_absent(&self) -> bool {
        self.resources.is_none()
    }

    pub(in crate::v2::engine) fn reclamation_budget(&self) -> usize {
        self.reclamation_budget
    }

    pub(in crate::v2::engine) fn has_cm_software_work(&self) -> bool {
        self.manager.cm.has_non_destruction_software_work()
    }

    pub(in crate::v2::engine) fn cm_software_snapshot(&self) -> CmSoftwareSnapshot {
        self.manager.cm_software_snapshot()
    }

    pub(in crate::v2::engine) fn has_cm_destruction_work(&self) -> bool {
        self.manager.cm.has_destruction_work()
    }

    pub(in crate::v2::engine) fn has_pending_cm_event(&self) -> bool {
        self.manager.has_pending_cm_event()
    }

    pub(in crate::v2::engine) fn cm_destruction_work_count(&self) -> usize {
        self.manager.cm.destruction_work_count()
    }

    pub(in crate::v2::engine) fn deadline_request_count(&self) -> usize {
        self.manager.deadline_request_count()
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

    pub(in crate::v2::engine) fn shutdown_work_pending(&self) -> bool {
        self.manager.shutdown_requested() && !self.shutdown_issuance_complete()
    }

    pub(in crate::v2::engine) fn can_finish(&self) -> bool {
        if !self.terminal_completion_ready || !self.shutdown_issuance_complete() {
            return false;
        }
        if self.manager.engine_runtime().is_none() {
            return false;
        }
        self.manager.pending_terminal_outcome().is_some() || self.terminal_state_drained()
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
    pub(in crate::v2::engine) fn turn_count(&self) -> usize {
        self.turns
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn exhaust_deadline_sequence_for_test(&mut self) {
        self.deadlines.exhaust_sequence_for_test();
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

    fn prepare_failure_scan(&mut self) {
        if self.failure_scan_started {
            return;
        }
        self.failure_scan_started = true;
        self.shutdown_cm = CmShutdownCursor::default();
        self.shutdown_connection_slot = 0;
        self.shutdown_connections_complete = false;
    }

    fn shutdown_issuance_complete(&self) -> bool {
        self.shutdown_started
            && self.shutdown_connections_complete
            && self.manager.cm.bounded_shutdown_complete(&self.shutdown_cm)
    }

    fn terminal_state_drained(&self) -> bool {
        !self.manager.has_cm_work()
            && self.manager.retained_cm_owner_count() == 0
            && self.manager.live_connection_count() == 0
    }

    pub(in crate::v2::engine) fn shutdown_snapshot(&self) -> CmShutdownSnapshot {
        self.manager.cm.bounded_shutdown_snapshot(
            self.manager.pending_terminal_outcome().is_some(),
            &self.shutdown_cm,
            self.cm_budget,
        )
    }

    pub(in crate::v2::engine) fn shutdown_connections_ready(&self) -> bool {
        !self.shutdown_connections_complete
    }

    pub(in crate::v2::engine) fn service_cm_shutdown_class(
        &mut self,
        class: CmShutdownClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> usize {
        let terminal = self.manager.pending_terminal_outcome();
        let outcome = terminal
            .clone()
            .unwrap_or_else(|| MemoizedTerminalResult::from_error(Error::DriverShutdown));
        self.manager.cm.service_bounded_shutdown_class(
            &self.manager,
            &outcome,
            terminal.is_some(),
            &mut self.shutdown_cm,
            class,
            budget,
            actions,
        )
    }

    pub(in crate::v2::engine) fn service_shutdown_connections(
        &mut self,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> usize {
        let terminal = self.manager.pending_terminal_outcome();
        let mut processed = 0;
        while processed < budget && actions.remaining() >= 8 && !self.shutdown_connections_complete
        {
            let (connections, next, complete, scanned) = self
                .manager
                .connections
                .scan_occupied(self.shutdown_connection_slot, 1);
            self.shutdown_connection_slot = next;
            self.shutdown_connections_complete = complete;
            for connection in connections {
                self.manager
                    .begin_connection_close_into(&connection, actions);
                if let Some(outcome) = terminal.as_ref() {
                    if connection.retain_bundle_for_engine_failure() {
                        self.manager.track_connection_quarantine(connection.token);
                    }
                    if let Some(event) = self
                        .manager
                        .finalize_connection_engine(&connection, outcome)
                    {
                        actions.push_event(event);
                    }
                    connection.wake_close_into(actions);
                }
            }
            if scanned == 0 {
                break;
            }
            processed += scanned;
        }
        processed
    }

    #[cfg(test)]
    fn service_deadlines_for_test(&mut self) -> Result<(usize, bool)> {
        let now = Instant::now();
        let requests = self.service_deadline_requests(self.reclamation_budget)?;
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let due =
            self.service_due_deadlines_into(now, self.reclamation_budget - requests, &mut actions)?;
        actions.publish();
        Ok((
            requests + due,
            self.manager.has_deadline_requests()
                || self.deadlines.next().is_some_and(|at| at <= now),
        ))
    }

    pub(in crate::v2::engine) fn service_cm_software_class(
        &mut self,
        class: CmSoftwareClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        self.manager.service_cm_software_class(
            self.resources
                .as_ref()
                .map(SessionProgressResources::engine),
            class,
            budget,
            actions,
        )
    }

    pub(in crate::v2::engine) fn service_cm_events(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<(usize, ReadinessRegistration, bool)> {
        let Some(resources) = self.resources.as_ref() else {
            return Ok((0, ReadinessRegistration::NotRequired, true));
        };
        let resources = resources.engine();
        let mut processed = 0;
        while processed < budget
            && actions.remaining() >= 8
            && self.manager.try_process_cm_event(resources, actions)?
        {
            processed += 1;
        }
        if processed == budget {
            return Ok((processed, ReadinessRegistration::Incomplete, false));
        }
        if actions.remaining() < 8 {
            return Ok((processed, ReadinessRegistration::Incomplete, false));
        }
        if mode != CompletionMode::Readiness {
            return Ok((processed, ReadinessRegistration::NotRequired, true));
        }
        let async_fd = resources
            .cm_async_fd
            .as_ref()
            .ok_or_else(|| Error::InvalidConfig("readiness engine has no CM AsyncFd".into()))?;
        match poll_readiness_events(
            cx,
            1,
            |cx| match async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => Poll::Ready(Ok(guard)),
                Poll::Ready(Err(error)) => Poll::Ready(Err(Error::Verbs(error))),
                Poll::Pending => Poll::Pending,
            },
            |guard| guard.clear_ready(),
            || {
                if actions.remaining() < 8 {
                    Ok(false)
                } else {
                    self.manager.try_process_cm_event(resources, actions)
                }
            },
        ) {
            Poll::Ready(result) => {
                let extra = result?;
                Ok((
                    processed + extra,
                    ReadinessRegistration::Incomplete,
                    extra == 0,
                ))
            }
            Poll::Pending => Ok((
                processed,
                ReadinessRegistration::RegisteredAndRechecked,
                true,
            )),
        }
    }

    pub(in crate::v2::engine) fn service_cm_destructions(
        &mut self,
        budget: usize,
        cm_drained: bool,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        if !cm_drained {
            return Ok(0);
        }
        let Some(resources) = self.resources.as_ref() else {
            return Ok(0);
        };
        let resources = resources.engine();
        self.manager
            .service_deferred_cm_destructions(budget, actions, || {
                self.manager.cm.defer_one_event(resources)
            })
    }

    pub(in crate::v2::engine) fn service_deadline_requests(
        &mut self,
        budget: usize,
    ) -> Result<usize> {
        let mut consumed = 0;
        for request in self.manager.take_deadline_requests(budget) {
            self.deadlines
                .push(
                    request.at,
                    SessionDeadline {
                        kind: request.kind,
                        token: request.token,
                    },
                )
                .map_err(|_| {
                    Error::InvalidConfig("session deadline insertion sequence exhausted".into())
                })?;
            consumed += 1;
        }
        Ok(consumed)
    }

    pub(in crate::v2::engine) fn service_due_deadlines_into(
        &mut self,
        _now: Instant,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        let mut consumed = 0;
        while consumed < budget {
            let Some(deadline) = self.ready_deadlines.front().copied() else {
                break;
            };
            let required_actions = match deadline.kind {
                DeadlineKind::EngineShutdown => 0,
                DeadlineKind::ConnectionDrain => 2,
            };
            if !actions.can_accept(required_actions) {
                break;
            }
            let deadline = self
                .ready_deadlines
                .pop_front()
                .expect("peeked session deadline remains at queue head");
            match deadline {
                SessionDeadline {
                    kind: DeadlineKind::EngineShutdown,
                    ..
                } => {
                    if let Some(failure) = self.manager.shutdown_deadline_failure() {
                        return Err(failure);
                    }
                }
                SessionDeadline {
                    kind: DeadlineKind::ConnectionDrain,
                    token,
                    ..
                } => self.manager.handle_connection_drain_deadline_into(
                    crate::v2::engine::registry::ConnectionToken::decode(token),
                    actions,
                ),
            }
            consumed += 1;
        }
        Ok(consumed)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_due_deadlines(
        &mut self,
        now: Instant,
        budget: usize,
    ) -> Result<usize> {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        self.prepare_due_deadline_snapshot(now);
        let result = self.service_due_deadlines_into(now, budget, &mut actions);
        actions.publish();
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionDeadline {
    kind: DeadlineKind,
    token: u64,
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
    async fn deadline_ingress_is_deferred_from_due_service_snapshot() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let mut progress =
            SessionReactorSources::new(Arc::clone(&engine.shared.session), None, 1, 1);
        engine.shared.session.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            1,
            std::time::Duration::ZERO,
        );
        let now = Instant::now();
        assert_eq!(progress.due_deadline_count(now), 0);
        assert_eq!(progress.service_deadline_requests(1).unwrap(), 1);
        assert_eq!(progress.service_due_deadlines(now, 0).unwrap(), 0);
        assert_eq!(progress.service_due_deadlines(now, 1).unwrap(), 1);

        drop(driver);
    }

    #[test]
    fn session_deadline_payloads_preserve_equal_time_order() {
        let now = Instant::now();
        let mut deadlines = DeadlineQueue::default();
        deadlines
            .push(
                now,
                SessionDeadline {
                    kind: DeadlineKind::ConnectionDrain,
                    token: 1,
                },
            )
            .unwrap();
        deadlines
            .push(
                now,
                SessionDeadline {
                    kind: DeadlineKind::EngineShutdown,
                    token: 2,
                },
            )
            .unwrap();

        assert_eq!(deadlines.pop_one_due(now).unwrap().token, 1);
        assert_eq!(deadlines.pop_one_due(now).unwrap().token, 2);
    }

    #[test]
    fn connection_deadline_stays_on_owner_queue_without_two_action_leaves() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let mut progress =
            SessionReactorSources::new(Arc::clone(&engine.shared.session), None, 1, 1);
        progress.ready_deadlines.push_back(SessionDeadline {
            kind: DeadlineKind::ConnectionDrain,
            token: 1,
        });
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        for _ in 0..crate::v2::engine::reactor::REACTOR_ACTION_BUDGET - 1 {
            actions.push_operation(|| {});
        }

        assert_eq!(
            progress
                .service_due_deadlines_into(Instant::now(), 1, &mut actions)
                .unwrap(),
            0
        );
        assert_eq!(progress.ready_deadlines.len(), 1);
        assert_eq!(progress.ready_deadlines.front().unwrap().token, 1);
        drop(driver);
    }

    #[tokio::test(start_paused = true)]
    async fn split_session_deadline_sources_preserve_the_aggregate_budget() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let mut progress =
            SessionReactorSources::new(Arc::clone(&engine.shared.session), None, 1, 2);
        engine.shared.session.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            1,
            std::time::Duration::ZERO,
        );
        let now = Instant::now();
        let requests = progress.service_deadline_requests(1).unwrap();
        let deadlines = progress.service_due_deadlines(now, 1).unwrap();
        assert_eq!(requests + deadlines, 2);
        drop(driver);
    }

    #[test]
    fn sustained_session_sources_alternate_and_transfer_unused_budget() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let mut progress =
            SessionReactorSources::new(Arc::clone(&engine.shared.session), None, 1, 4);
        let now = Instant::now();
        for token in 10..=11 {
            progress
                .deadlines
                .push(
                    now,
                    SessionDeadline {
                        kind: DeadlineKind::ConnectionDrain,
                        token,
                    },
                )
                .unwrap();
        }
        for token in 1..=2 {
            engine.shared.session.schedule_deadline(
                DeadlineKind::ConnectionDrain,
                token,
                std::time::Duration::ZERO,
            );
        }

        let requests = progress.service_deadline_requests(2).unwrap();
        let deadlines = progress.service_due_deadlines(now, 2).unwrap();
        assert_eq!(requests + deadlines, 4);

        let mut due_only =
            SessionReactorSources::new(Arc::clone(&engine.shared.session), None, 1, 3);
        for token in 20..=22 {
            due_only
                .deadlines
                .push(
                    now,
                    SessionDeadline {
                        kind: DeadlineKind::ConnectionDrain,
                        token,
                    },
                )
                .unwrap();
        }
        assert_eq!(due_only.service_due_deadlines(now, 3).unwrap(), 3);
        drop(driver);
    }

    #[test]
    fn deadline_sequence_exhaustion_maps_to_session_configuration_error() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        driver.reactor.session.deadlines.exhaust_sequence_for_test();
        engine.shared.session.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            7,
            std::time::Duration::ZERO,
        );
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let error = match driver
            .reactor
            .session
            .turn(CompletionMode::Polling, &mut cx)
        {
            Ok(_) => panic!("exhausted session deadline sequence must fail"),
            Err(error) => error,
        };

        assert!(
            matches!(error, Error::InvalidConfig(message) if message.contains("session deadline"))
        );
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
            .reactor
            .session
            .turn(CompletionMode::Polling, &mut cx)
            .unwrap();
        let closed = connections
            .iter()
            .filter(|connection| connection.state.close_started())
            .count();
        assert!(closed > 0 && closed < connections.len());
        assert!(first.units_consumed <= 48);
        assert!(first.immediate_work);
        assert!(!driver.reactor.session.can_finish());

        drop(connections);
        drop(driver);
    }

    #[tokio::test]
    async fn idle_shutdown_becomes_finishable_only_from_the_bounded_turn() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        engine.shared.request_shutdown();
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let mut ready = false;
        for _ in 0..4 {
            let report = driver
                .reactor
                .session
                .turn(CompletionMode::Polling, &mut cx)
                .unwrap();
            assert!(report.units_consumed <= 48);
            if driver.reactor.session.can_finish() {
                ready = true;
                break;
            }
        }
        assert!(ready);
        assert!(driver.reactor.session.can_finish());

        engine.shared.finish(MemoizedTerminalResult::success());
        drop(driver);
    }

    #[tokio::test]
    async fn failure_terminalizes_connections_across_bounded_turns() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connections = engine
            .shared
            .test_driver
            .install_idle_connections(&engine.shared, 64)
            .unwrap();
        engine
            .shared
            .begin_driver_failure(Error::InvalidConfig("bounded failure".into()));
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let first = driver
            .reactor
            .session
            .turn(CompletionMode::Polling, &mut cx)
            .unwrap();
        let terminalized = connections
            .iter()
            .filter(|connection| connection.state.close_state().raw_outcome().is_some())
            .count();
        assert!(terminalized > 0 && terminalized < connections.len());
        assert!(first.units_consumed <= 32);
        assert!(first.immediate_work);

        let mut ready = driver.reactor.session.can_finish();
        for _ in 0..8 {
            if ready {
                break;
            }
            let report = driver
                .reactor
                .session
                .turn(CompletionMode::Polling, &mut cx)
                .unwrap();
            assert!(report.units_consumed <= 32);
            ready = driver.reactor.session.can_finish();
        }
        assert!(ready);
        assert!(
            connections.iter().all(|connection| connection
                .state
                .close_state()
                .raw_outcome()
                .is_some())
        );

        engine
            .shared
            .finish_after_owner_cleanup(MemoizedTerminalResult::from_error(Error::InvalidConfig(
                "bounded failure".into(),
            )));
        drop(connections);
        drop(driver);
    }

    #[tokio::test]
    async fn failure_restarts_terminal_scan_after_partial_graceful_shutdown() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connections = engine
            .shared
            .test_driver
            .install_idle_connections(&engine.shared, 64)
            .unwrap();
        engine.shared.request_shutdown();
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let graceful = driver
            .reactor
            .session
            .turn(CompletionMode::Polling, &mut cx)
            .unwrap();
        assert!(graceful.immediate_work);
        assert!(driver.reactor.session.shutdown_connection_slot > 0);

        engine
            .shared
            .begin_driver_failure(Error::InvalidConfig("late failure".into()));
        let mut ready = false;
        for _ in 0..12 {
            driver
                .reactor
                .session
                .turn(CompletionMode::Polling, &mut cx)
                .unwrap();
            if driver.reactor.session.can_finish() {
                ready = true;
                break;
            }
        }
        assert!(ready);
        assert!(driver.reactor.session.failure_scan_started);
        assert!(
            connections.iter().all(|connection| connection
                .state
                .close_state()
                .raw_outcome()
                .is_some())
        );

        engine
            .shared
            .finish_after_owner_cleanup(MemoizedTerminalResult::from_error(Error::InvalidConfig(
                "late failure".into(),
            )));
        drop(connections);
        drop(driver);
    }
}
