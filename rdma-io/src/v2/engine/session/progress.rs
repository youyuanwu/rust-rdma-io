//! Bounded CM, lifecycle-deadline, and shutdown progress.

use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tokio::time::Instant;

use super::cm::CmShutdownCursor;
use super::cm::CmState;
use super::registry::ConnectionRegistry;
use super::{
    CmShutdownClass, CmShutdownSnapshot, CmSoftwareClass, CmSoftwareSnapshot, DeadlineKind,
    SessionContext,
};
use crate::v2::engine::config::CompletionMode;
use crate::v2::engine::io_core::IoCoreEffects;
use crate::v2::engine::lifecycle::MemoizedTerminalResult;
use crate::v2::engine::progress::ReadinessRegistration;
use crate::v2::engine::resources::EngineReactorResources;
use crate::v2::engine::scheduler::DeadlineQueue;
use crate::v2::error::{Error, Result};

pub(in crate::v2::engine) struct SessionReactorSources {
    pub(in crate::v2::engine) manager: SessionContext,
    pub(in crate::v2::engine) cm: CmState,
    pub(in crate::v2::engine) connections: ConnectionRegistry,
    deadlines: DeadlineQueue<SessionDeadline>,
    ready_deadlines: std::collections::VecDeque<SessionDeadline>,
    shutdown_requested: bool,
    terminal_outcome: Option<MemoizedTerminalResult>,
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
    shutdown_deadline: std::time::Duration,
}

impl SessionReactorSources {
    pub(in crate::v2::engine) fn synchronously_service_listener_driver_drop(
        &mut self,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineReactorResources>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let listener_steps = self
            .cm
            .pending_lifecycle_work_count()
            .saturating_mul(4)
            .max(1);
        for _ in 0..listener_steps {
            if self.cm.listener_work_count() == 0 {
                break;
            }
            let result = self.cm.service_software_class_into(
                &mut self.connections,
                &self.manager,
                io_core,
                resources,
                CmSoftwareClass::ListenerWork,
                1,
                actions,
            );
            if let Err(error) = result {
                tracing::warn!(%error, "listener cleanup remained quarantined during driver drop");
                break;
            }
        }

        let Some(resources) = resources else {
            return;
        };
        let destruction_steps = self
            .cm
            .destruction_work_count()
            .saturating_mul(4)
            .saturating_add(self.cm_budget)
            .max(1);
        for _ in 0..destruction_steps {
            if !self.cm.has_destruction_work() {
                break;
            }
            let result = self.cm.service_cm_destructions_into(
                &mut self.connections,
                io_core,
                resources,
                1,
                actions,
            );
            if let Err(error) = result {
                tracing::warn!(%error, "CM destruction remained quarantined during driver drop");
                break;
            }
        }
    }

    pub(in crate::v2::engine) fn commit_io_effects_into(
        &mut self,
        effects: IoCoreEffects,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        super::apply_io_effects(&self.manager, &mut self.connections, effects).append_to(actions);
    }

    pub(in crate::v2::engine) fn enqueue_completion_with_core(
        &mut self,
        io_core: &mut crate::v2::engine::io_core::IoState,
        completion: crate::wc::WorkCompletion,
    ) -> Option<crate::v2::engine::registry::ConnectionToken> {
        let _admission =
            crate::v2::engine::registry::read_unpoison(&self.manager.frontend.admission);
        let pending = io_core.prepare_completion(completion)?;
        let identity = pending.identity();
        if !matches!(
            self.connections.lookup(identity.connection),
            super::Lookup::Occupied(_)
        ) {
            io_core.reject_cqe(crate::v2::engine::io_core::CqeReject::StaleConnection);
            return None;
        }
        let live = self
            .connections
            .prove_live_io(identity.connection, identity.qp_num);
        self.connections
            .with_connection_io_mut(identity.connection, |connection, connection_io, _poster| {
                io_core.enqueue_prepared_completion(pending, live, connection, connection_io)
            })
            .flatten()
    }

    pub(in crate::v2::engine) fn dispatch_connection_completions_with_core(
        &mut self,
        io_core: &mut crate::v2::engine::io_core::IoState,
        token: crate::v2::engine::registry::ConnectionToken,
        quantum: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> (usize, bool) {
        let Some((processed, remains_ready, effects)) =
            self.connections
                .with_connection_io_mut(token, |connection, connection_io, _poster| {
                    io_core.dispatch_connection_completions(connection, connection_io, quantum)
                })
        else {
            return (0, false);
        };
        self.commit_io_effects_into(effects, actions);
        (processed, remains_ready)
    }

    pub(in crate::v2::engine) fn handle_reclamation_deadline_with_core(
        &mut self,
        io_core: &mut crate::v2::engine::io_core::IoState,
        operation: crate::v2::engine::registry::OperationToken,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let Some(connection) = io_core.operation_connection(operation) else {
            return;
        };
        let Some(effects) =
            self.connections
                .with_connection_io_mut(connection, |_io, connection_io, _poster| {
                    io_core.handle_reclamation_deadline(operation, connection_io)
                })
        else {
            return;
        };
        self.commit_io_effects_into(effects, actions);
    }

    pub(in crate::v2::engine) fn new(
        manager: SessionContext,
        connection_admission: Arc<tokio::sync::Semaphore>,
        cm_budget: usize,
        reclamation_budget: usize,
        shutdown_deadline: std::time::Duration,
    ) -> Self {
        Self {
            cm: CmState::new(manager.max_live_connections())
                .expect("validated listener registry capacity"),
            connections: ConnectionRegistry::new_with_admission(
                manager.max_live_connections(),
                connection_admission,
            )
            .expect("validated connection registry capacity"),
            manager,
            deadlines: DeadlineQueue::default(),
            ready_deadlines: std::collections::VecDeque::new(),
            shutdown_requested: false,
            terminal_outcome: None,
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
            shutdown_deadline,
        }
    }

    pub(in crate::v2::engine) fn begin_turn(
        &mut self,
        shutting_down: bool,
        terminal_outcome: Option<MemoizedTerminalResult>,
    ) -> Result<(bool, bool)> {
        #[cfg(test)]
        {
            self.turns = self.turns.saturating_add(1);
        }
        self.shutdown_requested = shutting_down;
        self.terminal_outcome = terminal_outcome;
        let terminal_failure = self.terminal_outcome.is_some();
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
        _resources: Option<&EngineReactorResources>,
    ) {
        if terminal_failure
            && self.shutdown_issuance_complete()
            && self.cm.listener_work_count() == 0
        {
            self.terminal_completion_ready = true;
        } else if shutting_down
            && observed_would_block
            && self.shutdown_issuance_complete()
            && self.terminal_state_drained()
        {
            #[cfg(any(test, feature = "test-hooks"))]
            if let Some(resources) = _resources {
                crate::test_support::destruction::record(
                    crate::test_support::destruction::DestructionKind::CmFinalDrainToWouldBlock,
                    resources.cm_event_channel.as_raw() as usize,
                );
            }
            self.terminal_completion_ready = true;
        }
    }

    pub(in crate::v2::engine) fn cm_budget(&self) -> usize {
        self.cm_budget
    }

    pub(in crate::v2::engine) fn reclamation_budget(&self) -> usize {
        self.reclamation_budget
    }

    pub(in crate::v2::engine) fn has_cm_software_work(&self) -> bool {
        self.cm.has_non_destruction_software_work(&self.connections)
    }

    pub(in crate::v2::engine) fn cm_software_snapshot(&self) -> CmSoftwareSnapshot {
        self.cm.software_snapshot(&self.connections)
    }

    pub(in crate::v2::engine) fn has_cm_destruction_work(&self) -> bool {
        self.cm.has_destruction_work()
    }

    pub(in crate::v2::engine) fn has_pending_cm_event(&self) -> bool {
        self.cm.has_pending_event()
    }

    pub(in crate::v2::engine) fn cm_destruction_work_count(&self) -> usize {
        self.cm.destruction_work_count()
    }

    pub(in crate::v2::engine) fn deadline_request_count(&self) -> usize {
        self.connections.deadline_request_count()
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
        self.shutdown_requested && !self.shutdown_issuance_complete()
    }

    pub(in crate::v2::engine) fn can_finish(&self) -> bool {
        if !self.terminal_completion_ready || !self.shutdown_issuance_complete() {
            return false;
        }
        self.terminal_outcome.is_some() || self.terminal_state_drained()
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
        self.cm.start_bounded_shutdown();
        self.connections.schedule_deadline(
            super::DeadlineKind::EngineShutdown,
            0,
            self.shutdown_deadline,
        );
        #[cfg(test)]
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
            && self
                .cm
                .bounded_shutdown_complete(&self.connections, &self.shutdown_cm)
    }

    fn terminal_state_drained(&self) -> bool {
        !self.cm.has_software_work(&self.connections)
            && self.cm.retained_owner_count(&self.connections) == 0
            && self.connections.live() == 0
    }

    pub(in crate::v2::engine) fn shutdown_snapshot(&self) -> CmShutdownSnapshot {
        self.cm.bounded_shutdown_snapshot(
            &self.connections,
            self.terminal_outcome.is_some(),
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
        let terminal = self.terminal_outcome.clone();
        let outcome = terminal
            .clone()
            .unwrap_or_else(|| MemoizedTerminalResult::from_error(Error::DriverShutdown));
        self.cm.service_bounded_shutdown_class(
            &mut self.connections,
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
        io_core: &mut crate::v2::engine::io_core::IoState,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> usize {
        let terminal = self.terminal_outcome.clone();
        let mut processed = 0;
        while processed < budget && actions.remaining() >= 8 && !self.shutdown_connections_complete
        {
            let (connections, next, complete, scanned) = self
                .connections
                .scan_occupied(self.shutdown_connection_slot, 1);
            self.shutdown_connection_slot = next;
            self.shutdown_connections_complete = complete;
            for token in connections {
                Self::begin_connection_close_into(
                    &self.manager,
                    &mut self.cm,
                    &mut self.connections,
                    token,
                    io_core,
                    actions,
                );
                if let Some(outcome) = terminal.as_ref() {
                    let accepted = self.connections.accepted_count(token);
                    let retain = self
                        .connections
                        .with_connection(token, |connection| {
                            connection.retain_bundle_for_engine_failure(accepted)
                        })
                        .unwrap_or(false);
                    if retain {
                        self.connections.track_bundle_quarantine(token);
                    }
                    let event = self.connections.finalize_connection_engine(token, outcome);
                    if let Some(event) = event {
                        actions.push_event(event);
                    }
                    self.connections.wake_close_into(token, actions);
                }
            }
            if scanned == 0 {
                break;
            }
            processed += scanned;
        }
        processed
    }

    pub(in crate::v2::engine) fn service_cm_software_class(
        &mut self,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineReactorResources>,
        class: CmSoftwareClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        self.cm.service_software_class_into(
            &mut self.connections,
            &self.manager,
            io_core,
            resources,
            class,
            budget,
            actions,
        )
    }

    pub(in crate::v2::engine) fn service_cm_events(
        &mut self,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineReactorResources>,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<(usize, ReadinessRegistration, bool)> {
        let Some(resources) = resources else {
            return Ok((0, ReadinessRegistration::NotRequired, true));
        };
        let mut processed = 0;
        while processed < budget
            && actions.remaining() >= 8
            && self.cm.try_process_event(
                &mut self.connections,
                &self.manager,
                io_core,
                resources,
                actions,
            )?
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
                    self.cm.try_process_event(
                        &mut self.connections,
                        &self.manager,
                        io_core,
                        resources,
                        actions,
                    )
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
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineReactorResources>,
        budget: usize,
        cm_drained: bool,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        if !cm_drained {
            return Ok(0);
        }
        let Some(resources) = resources else {
            return Ok(0);
        };
        self.cm.service_cm_destructions_into(
            &mut self.connections,
            io_core,
            resources,
            budget,
            actions,
        )
    }

    pub(in crate::v2::engine) fn service_deadline_requests(
        &mut self,
        budget: usize,
    ) -> Result<usize> {
        let mut consumed = 0;
        for request in self.connections.take_deadline_requests(budget) {
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
        io_core: &mut crate::v2::engine::io_core::IoState,
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
                    let retained_bundles = self
                        .connections
                        .admission_snapshot()
                        .live
                        .max(self.cm.retained_session_owner_count());
                    let outstanding_operations = io_core.accepted_count();
                    let pending_routes = self
                        .connections
                        .admission_snapshot()
                        .live
                        .saturating_add(self.cm.pending_lifecycle_work_count());
                    if retained_bundles != 0 || outstanding_operations != 0 || pending_routes != 0 {
                        return Err(Error::EngineWedged {
                            retained_bundles,
                            outstanding_operations,
                            cq_debt: outstanding_operations,
                        });
                    }
                }
                SessionDeadline {
                    kind: DeadlineKind::ConnectionDrain,
                    token,
                    ..
                } => Self::handle_connection_drain_deadline_into(
                    &self.manager,
                    &mut self.connections,
                    io_core,
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
        io_core: &mut crate::v2::engine::io_core::IoState,
        now: Instant,
        budget: usize,
    ) -> Result<usize> {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        self.prepare_due_deadline_snapshot(now);
        let result = self.service_due_deadlines_into(now, budget, io_core, &mut actions);
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
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let progress = &mut driver.reactor.session;
        progress.connections.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            1,
            std::time::Duration::ZERO,
        );
        let now = Instant::now();
        assert_eq!(progress.due_deadline_count(now), 0);
        assert_eq!(progress.service_deadline_requests(1).unwrap(), 1);
        assert_eq!(
            progress
                .service_due_deadlines(driver.reactor.io.core_mut(), now, 0)
                .unwrap(),
            0
        );
        assert_eq!(
            progress
                .service_due_deadlines(driver.reactor.io.core_mut(), now, 1)
                .unwrap(),
            1
        );

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
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let progress = &mut driver.reactor.session;
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
                .service_due_deadlines_into(
                    Instant::now(),
                    1,
                    driver.reactor.io.core_mut(),
                    &mut actions,
                )
                .unwrap(),
            0
        );
        assert_eq!(progress.ready_deadlines.len(), 1);
        assert_eq!(progress.ready_deadlines.front().unwrap().token, 1);
        drop(driver);
    }

    #[tokio::test(start_paused = true)]
    async fn split_session_deadline_sources_preserve_the_aggregate_budget() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let progress = &mut driver.reactor.session;
        progress.connections.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            1,
            std::time::Duration::ZERO,
        );
        let now = Instant::now();
        let requests = progress.service_deadline_requests(1).unwrap();
        let deadlines = progress
            .service_due_deadlines(driver.reactor.io.core_mut(), now, 1)
            .unwrap();
        assert_eq!(requests + deadlines, 2);
        drop(driver);
    }

    #[test]
    fn sustained_session_sources_alternate_and_transfer_unused_budget() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let progress = &mut driver.reactor.session;
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
            progress.connections.schedule_deadline(
                DeadlineKind::ConnectionDrain,
                token,
                std::time::Duration::ZERO,
            );
        }

        let requests = progress.service_deadline_requests(2).unwrap();
        let deadlines = progress
            .service_due_deadlines(driver.reactor.io.core_mut(), now, 2)
            .unwrap();
        assert_eq!(requests + deadlines, 4);

        let due_only = &mut driver.reactor.session;
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
        assert_eq!(
            due_only
                .service_due_deadlines(driver.reactor.io.core_mut(), now, 3)
                .unwrap(),
            3
        );
        drop(driver);
    }

    #[test]
    fn deadline_sequence_exhaustion_maps_to_session_configuration_error() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        driver.reactor.session.deadlines.exhaust_sequence_for_test();
        driver.reactor.session.connections.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            7,
            std::time::Duration::ZERO,
        );
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let error =
            match driver
                .reactor
                .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
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
            .install_idle_connections(&mut driver.reactor.session, 64)
            .unwrap();
        engine.shared.request_shutdown();
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let first = driver
            .reactor
            .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
            .unwrap();
        let closed = connections
            .iter()
            .filter(|connection| {
                driver
                    .reactor
                    .session
                    .connections
                    .close_started(connection.session_token())
            })
            .count();
        assert!(closed > 0 && closed < connections.len());
        assert!(first);
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
            driver
                .reactor
                .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
                .unwrap();
            if driver.reactor.session.can_finish() {
                ready = true;
                break;
            }
        }
        assert!(ready);
        assert!(driver.reactor.session.can_finish());

        driver
            .reactor
            .finish_for_test(&engine.shared, MemoizedTerminalResult::success());
        drop(driver);
    }

    #[tokio::test]
    async fn failure_terminalizes_connections_across_bounded_turns() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connections = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 64)
            .unwrap();
        engine
            .shared
            .begin_driver_failure(Error::InvalidConfig("bounded failure".into()));
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let first = driver
            .reactor
            .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
            .unwrap();
        let terminalized = connections
            .iter()
            .filter(|connection| connection.state.close_state().raw_outcome().is_some())
            .count();
        assert!(terminalized > 0 && terminalized < connections.len());
        assert!(first);

        let mut ready = driver.reactor.session.can_finish();
        for _ in 0..128 {
            if ready {
                break;
            }
            driver
                .reactor
                .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
                .unwrap();
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

        driver.reactor.finish_after_owner_cleanup_for_test(
            &engine.shared,
            MemoizedTerminalResult::from_error(Error::InvalidConfig("bounded failure".into())),
        );
        drop(connections);
        drop(driver);
    }

    #[tokio::test]
    async fn failure_restarts_terminal_scan_after_partial_graceful_shutdown() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connections = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 64)
            .unwrap();
        engine.shared.request_shutdown();
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);

        let graceful = driver
            .reactor
            .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
            .unwrap();
        assert!(graceful);
        assert!(driver.reactor.session.shutdown_connection_slot > 0);

        engine
            .shared
            .begin_driver_failure(Error::InvalidConfig("late failure".into()));
        let mut ready = false;
        for _ in 0..128 {
            driver
                .reactor
                .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
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

        driver.reactor.finish_after_owner_cleanup_for_test(
            &engine.shared,
            MemoizedTerminalResult::from_error(Error::InvalidConfig("late failure".into())),
        );
        drop(connections);
        drop(driver);
    }
}
