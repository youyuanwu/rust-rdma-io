//! Driver-owned command, I/O, connection-lifecycle, and session progress.
//!
//! Connection and listener identity, the shared CM context route and
//! destruction service, admission, deadlines, shutdown, terminal state,
//! retirement, and quarantine live under this reactor.

mod action;
pub(super) mod command;
pub(super) mod completion;
mod scheduler;

use std::sync::{Arc, Mutex, OnceLock};
use std::task::Context as TaskContext;
use tokio::time::Instant;

use super::EngineFrontendRoot;
use super::config::CompletionMode;
use super::diagnostics::{PublishedDiagnostics, RdmaEngineDiagnostics};
use super::io_core::{IoDriverSignal, IoReactorSources, IoState};
use super::lifecycle::EngineLifecycleState;
use super::progress::ReadinessRegistration;
use super::resources::EngineReactorResources;
use super::session::{CmShutdownClass, CmSoftwareClass, SessionManager, SessionReactorSources};

#[cfg(test)]
pub(super) use action::REACTOR_ACTION_BUDGET;
pub(super) use action::{DeferredProtocolActions, ReactorActions};
pub(super) use command::CommandIngress;
use scheduler::{ReactorScheduler, ReactorSource};

/// Driver-owned scheduling state for one bounded top-level reactor turn.
///
/// All mutable engine runtime state is owned through this value. Shared
/// frontends retain only typed ingress, immutable policy, and resource-free
/// completion observers.
pub(super) struct EngineReactor {
    pub(super) io: IoReactorSources,
    pub(super) session: SessionReactorSources,
    pub(super) lifecycle: EngineLifecycleState,
    scheduler: ReactorScheduler,
    resources: Option<EngineReactorResources>,
    #[cfg(test)]
    served_sources: Vec<ReactorSource>,
    #[cfg(test)]
    last_action_count: usize,
}

pub(super) struct ReactorTurn {
    pub(super) actions: ReactorActions,
    pub(super) requires_repoll: bool,
}

pub(super) struct ReactorTurnFailure {
    pub(super) error: super::Error,
    pub(super) actions: ReactorActions,
}

impl EngineReactor {
    pub(super) fn new(
        shared: &Arc<EngineFrontendRoot>,
        manager: SessionManager,
        resources: Option<EngineReactorResources>,
    ) -> Self {
        let io_driver_signal: Arc<dyn IoDriverSignal> = Arc::new(super::EngineIoDriverSignal {
            work_signal: Arc::clone(&shared.work_signal),
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver: Arc::clone(&shared.test_driver),
        });
        let io_core = IoState::new_owned(
            shared.config.max_inflight_operations,
            shared.config.cq_capacity,
            shared.config.missing_cqe_deadline,
            shared.config.completion_dispatch_budget,
            Arc::clone(&shared.session.admission),
            io_driver_signal,
        )
        .expect("validated engine I/O configuration");
        Self {
            io: IoReactorSources::new(
                io_core,
                shared.config.cq_completion_budget,
                shared.config.completion_dispatch_budget,
                shared.config.io_reclamation_budget,
                #[cfg(any(test, feature = "test-hooks"))]
                Arc::clone(&shared.test_driver),
            ),
            session: SessionReactorSources::new(
                manager,
                shared.commands.connection_admission(),
                shared.config.cm_event_budget,
                shared.config.session_reclamation_budget,
                shared.config.shutdown_deadline,
            ),
            lifecycle: EngineLifecycleState::new(),
            scheduler: ReactorScheduler::new(),
            resources,
            #[cfg(test)]
            served_sources: Vec::new(),
            #[cfg(test)]
            last_action_count: 0,
        }
    }

    pub(super) fn begin_driver_failure(
        &mut self,
        shared: &Arc<EngineFrontendRoot>,
        error: super::Error,
    ) {
        if self.lifecycle.begin_failure(error.clone()) {
            let admission = super::registry::write_unpoison(&shared.session.admission);
            shared.commands.close_admission_with(error);
            drop(admission);
            shared.start_shutdown_progress();
        }
    }

    pub(super) fn transition_running(&mut self, _shared: &EngineFrontendRoot) {
        self.lifecycle.transition_running();
    }

    fn finish_after_owner_cleanup_into(
        &mut self,
        shared: &EngineFrontendRoot,
        outcome: super::lifecycle::MemoizedTerminalResult,
        actions: &mut ReactorActions,
    ) {
        if self.lifecycle.finish(outcome.clone()) {
            shared.publish_terminal_into(outcome, actions);
        }
    }

    fn progress_driver_terminal(
        &mut self,
        shared: &EngineFrontendRoot,
        actions: &mut ReactorActions,
    ) -> bool {
        if !self.lifecycle.shutdown_requested()
            || shared.commands.has_pending()
            || !self.io.can_finish()
            || !self.session.can_finish()
        {
            return false;
        }
        let outcome = self
            .lifecycle
            .pending_terminal()
            .unwrap_or_else(super::lifecycle::MemoizedTerminalResult::success);
        self.finish_after_owner_cleanup_into(shared, outcome, actions);
        true
    }

    fn finish_driver_drop_into(
        &mut self,
        shared: &EngineFrontendRoot,
        outcome: super::lifecycle::MemoizedTerminalResult,
        actions: &mut ReactorActions,
    ) {
        if self.lifecycle.outcome().is_some() {
            return;
        }
        shared
            .commands
            .close_admission_with(outcome.error().unwrap_or(super::Error::DriverShutdown));
        self.io.core_mut().close_admission(outcome.error());
        let io_effects = self.io.core_mut().terminalize_operations(&outcome);
        let connections_to_wake = self.session.connections.occupied();
        self.session
            .manager
            .apply_terminal_io_effects(&mut self.session.connections, io_effects)
            .append_to(actions);
        shared.commands.drain_ordinary_into(
            outcome.error().unwrap_or(super::Error::DriverShutdown),
            actions,
        );
        for token in &connections_to_wake {
            let accepted = self.session.connections.accepted_count(*token);
            let retain = self
                .session
                .connections
                .with_connection(*token, |connection| {
                    connection.retain_bundle_for_engine_failure(accepted)
                })
                .unwrap_or(false);
            if outcome.is_error() && retain {
                self.session
                    .manager
                    .track_connection_quarantine(&mut self.session.connections, *token);
            }
            let event = if self.session.connections.is_quarantined(*token) {
                self.session.manager.finalize_quarantined_connection_engine(
                    &mut self.session.connections,
                    *token,
                    &outcome,
                )
            } else {
                self.session.manager.finalize_connection_engine(
                    &mut self.session.connections,
                    *token,
                    &outcome,
                )
            };
            if let Some(event) = event {
                actions.push_event(event);
            }
            self.session.connections.wake_close_into(*token, actions);
        }
        self.session
            .cm
            .terminalize_into(&mut self.session.connections, &outcome, actions);
        self.finish_after_owner_cleanup_into(shared, outcome, actions);
    }

    fn handle_driver_drop(&mut self, shared: &Arc<EngineFrontendRoot>) -> ReactorActions {
        let mut actions = ReactorActions::for_synchronous_driver_drop();
        if self.lifecycle.outcome().is_some() {
            return actions;
        }
        self.begin_driver_failure(shared, super::Error::DriverShutdown);
        self.session
            .synchronously_prepare_driver_drop(self.io.core_mut());
        let outstanding = self.io.core().accepted_count();
        let cm_owners = self
            .session
            .cm
            .retained_provider_owner_count()
            .max(self.session.connections.live());
        let error = if outstanding == 0 && cm_owners == 0 {
            super::Error::DriverShutdown
        } else {
            super::Error::EngineWedged {
                retained_bundles: self
                    .session
                    .connections
                    .admission_snapshot()
                    .live
                    .max(self.session.cm.retained_provider_owner_count())
                    .max(1),
                outstanding_operations: outstanding,
                cq_debt: outstanding,
            }
        };
        let outcome = super::lifecycle::MemoizedTerminalResult::from_error(error);
        self.session
            .cm
            .terminalize_into(&mut self.session.connections, &outcome, &mut actions);
        self.session.synchronously_service_listener_driver_drop(
            self.io.core_mut(),
            self.resources.as_ref(),
            &mut actions,
        );
        self.finish_driver_drop_into(shared, outcome, &mut actions);
        actions
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn finish_for_test(
        &mut self,
        shared: &Arc<EngineFrontendRoot>,
        outcome: super::lifecycle::MemoizedTerminalResult,
    ) {
        assert!(
            !outcome.is_connection_quarantined(),
            "ConnectionQuarantined is connection-local; no connection quarantine can terminate the engine driver"
        );
        let mut actions = ReactorActions::for_synchronous_driver_drop();
        self.finish_driver_drop_into(shared, outcome, &mut actions);
        self.publish_diagnostics(shared, self.session.connections.admission_snapshot());
        actions.publish();
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn finish_after_owner_cleanup_for_test(
        &mut self,
        shared: &Arc<EngineFrontendRoot>,
        outcome: super::lifecycle::MemoizedTerminalResult,
    ) {
        let mut actions = ReactorActions::default();
        self.finish_after_owner_cleanup_into(shared, outcome, &mut actions);
        self.publish_diagnostics(shared, self.session.connections.admission_snapshot());
        actions.publish();
    }

    #[allow(
        clippy::result_large_err,
        reason = "failure preserves the already-bounded action batch for publication"
    )]
    pub(super) fn turn(
        &mut self,
        shared: &Arc<EngineFrontendRoot>,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> std::result::Result<ReactorTurn, ReactorTurnFailure> {
        let mut actions = ReactorActions::default();
        macro_rules! try_turn {
            ($expression:expr) => {
                match $expression {
                    Ok(value) => value,
                    Err(error) => return Err(ReactorTurnFailure { error, actions }),
                }
            };
        }
        let now = Instant::now();
        if let Some(error) = shared.take_driver_failure() {
            self.begin_driver_failure(shared, error);
        }
        if shared.commands.shutdown_requested() && self.lifecycle.request_shutdown() {
            shared.start_shutdown_progress();
        }
        self.io
            .sync_lifecycle(shared.admission_error(), self.lifecycle.pending_terminal());
        let io_terminal = try_turn!(self.io.begin_turn());
        let (shutting_down, terminal_failure) = try_turn!(self.session.begin_turn(
            self.lifecycle.shutdown_requested(),
            self.lifecycle.pending_terminal(),
        ));
        let completion_count = self.io.prepare_completion_dispatch_snapshot();
        let io_reclamation_count = self.io.reclamation_request_count();
        let io_deadline_count = self.io.prepare_due_deadline_snapshot(now);
        let cm_software_snapshot = self.session.cm_software_snapshot();
        let cm_destruction_count = self.session.cm_destruction_work_count();
        let session_deadline_count = self.session.deadline_request_count();
        let session_due_count = self.session.prepare_due_deadline_snapshot(now);
        let shutdown_ready = self.session.shutdown_work_pending();
        let shutdown_snapshot = self.session.shutdown_snapshot();
        let shutdown_connections_ready = self.session.shutdown_connections_ready();
        let commands_ready = shared
            .commands
            .has_runnable(self.session.cm.listener_slot_available());

        let ready_sources = [
            (ReactorSource::Commands, commands_ready),
            (ReactorSource::Cq, !io_terminal),
            (
                ReactorSource::CompletionDispatch,
                !io_terminal && completion_count != 0,
            ),
            (
                ReactorSource::IoReclamation,
                !io_terminal && io_reclamation_count != 0,
            ),
            (
                ReactorSource::IoDeadline,
                !io_terminal && io_deadline_count != 0,
            ),
            (
                ReactorSource::CmCancellation,
                !terminal_failure && cm_software_snapshot.count(CmSoftwareClass::Cancellation) != 0,
            ),
            (
                ReactorSource::CmRetirement,
                !terminal_failure && cm_software_snapshot.count(CmSoftwareClass::Retirement) != 0,
            ),
            (
                ReactorSource::CmOutboundStart,
                !terminal_failure
                    && cm_software_snapshot.count(CmSoftwareClass::OutboundStart) != 0,
            ),
            (
                ReactorSource::CmListenStart,
                !terminal_failure && cm_software_snapshot.count(CmSoftwareClass::ListenStart) != 0,
            ),
            (
                ReactorSource::CmListenerWork,
                cm_software_snapshot.count(CmSoftwareClass::ListenerWork) != 0,
            ),
            (ReactorSource::CmEvent, !terminal_failure),
            (
                ReactorSource::CmDestruction,
                !terminal_failure && cm_destruction_count != 0,
            ),
            (
                ReactorSource::SessionDeadlineIngress,
                !terminal_failure && session_deadline_count != 0,
            ),
            (
                ReactorSource::SessionDeadline,
                !terminal_failure && session_due_count != 0,
            ),
            (
                ReactorSource::ShutdownPendingOutbound,
                shutdown_ready && shutdown_snapshot.count(CmShutdownClass::PendingOutbound) != 0,
            ),
            (
                ReactorSource::ShutdownRoutes,
                shutdown_ready && shutdown_snapshot.count(CmShutdownClass::Routes) != 0,
            ),
            (
                ReactorSource::ShutdownPendingListen,
                shutdown_ready && shutdown_snapshot.count(CmShutdownClass::PendingListen) != 0,
            ),
            (
                ReactorSource::ShutdownListeners,
                shutdown_ready && shutdown_snapshot.count(CmShutdownClass::Listeners) != 0,
            ),
            (
                ReactorSource::ShutdownRetainedListeners,
                shutdown_ready && shutdown_snapshot.count(CmShutdownClass::RetainedListeners) != 0,
            ),
            (
                ReactorSource::ShutdownConnections,
                shutdown_ready && shutdown_connections_ready,
            ),
            (ReactorSource::IoTerminal, io_terminal),
        ];
        let mut requires_repoll = false;
        let mut ready = self.scheduler.begin_turn(|source| {
            ready_sources
                .iter()
                .any(|(candidate, ready)| *candidate == source && *ready)
        });
        let mut observed_cm_would_block = self.resources.is_none();
        while let Some(source) = ready.pop_front() {
            #[cfg(test)]
            self.served_sources.push(source);
            match source {
                ReactorSource::Commands => {
                    let report = shared.commands.service_turn_into(
                        shared,
                        self.io.core_mut(),
                        &mut self.session,
                        &mut actions,
                    );
                    if report.has_more {
                        shared.work_signal.notify_reactor();
                    }
                    if report.shutdown_requested {
                        self.lifecycle.request_shutdown();
                    }
                    requires_repoll |= report.has_more || report.session_work;
                }
                ReactorSource::Cq => {
                    let (_, _, repoll) = try_turn!(self.io.service_cq(
                        &mut self.session,
                        self.resources.as_ref(),
                        mode,
                        cx,
                    ));
                    requires_repoll |= repoll;
                }
                ReactorSource::CompletionDispatch => {
                    let quantum = self
                        .io
                        .completion_dispatch_budget()
                        .min(actions.remaining() / 3);
                    if quantum == 0 {
                        requires_repoll = true;
                    } else {
                        let (_, more) = try_turn!(self.io.service_completion_dispatch(
                            quantum,
                            &mut self.session,
                            &mut actions,
                        ));
                        requires_repoll |= more;
                    }
                }
                ReactorSource::IoReclamation => {
                    let limit = io_reclamation_count.min(self.io.reclamation_budget());
                    let used = try_turn!(self.io.service_reclamation_requests(limit));
                    requires_repoll |= used == limit && self.io.reclamation_request_count() != 0;
                }
                ReactorSource::IoDeadline => {
                    let limit = io_deadline_count.min(self.io.reclamation_budget());
                    let used = self.io.service_reclamation_deadlines(
                        now,
                        limit,
                        &mut self.session,
                        &mut actions,
                    );
                    requires_repoll |= used == limit && self.io.due_deadline_count(now) != 0;
                }
                ReactorSource::CmCancellation
                | ReactorSource::CmRetirement
                | ReactorSource::CmOutboundStart
                | ReactorSource::CmListenStart
                | ReactorSource::CmListenerWork => {
                    let class = match source {
                        ReactorSource::CmCancellation => CmSoftwareClass::Cancellation,
                        ReactorSource::CmRetirement => CmSoftwareClass::Retirement,
                        ReactorSource::CmOutboundStart => CmSoftwareClass::OutboundStart,
                        ReactorSource::CmListenStart => CmSoftwareClass::ListenStart,
                        ReactorSource::CmListenerWork => CmSoftwareClass::ListenerWork,
                        _ => unreachable!("matched one CM software source"),
                    };
                    let limit = cm_software_snapshot
                        .count(class)
                        .min(self.session.cm_budget())
                        .min(actions.remaining() / 8);
                    if limit == 0 {
                        requires_repoll = true;
                    } else {
                        let used = try_turn!(self.session.service_cm_software_class(
                            self.io.core_mut(),
                            self.resources.as_ref(),
                            class,
                            limit,
                            &mut actions,
                        ));
                        requires_repoll |= used == limit && self.session.has_cm_software_work();
                    }
                }
                ReactorSource::CmEvent => {
                    let limit = self.session.cm_budget().min(actions.remaining() / 8);
                    if limit == 0 {
                        requires_repoll = true;
                    } else {
                        let (_, readiness, would_block) =
                            try_turn!(self.session.service_cm_events(
                                self.io.core_mut(),
                                self.resources.as_ref(),
                                mode,
                                cx,
                                limit,
                                &mut actions,
                            ));
                        requires_repoll |= readiness == ReadinessRegistration::Incomplete;
                        observed_cm_would_block = would_block;
                    }
                }
                ReactorSource::CmDestruction => {
                    let limit = cm_destruction_count
                        .min(self.session.cm_budget())
                        .min(actions.remaining() / 8)
                        .min(1);
                    if limit == 0 {
                        requires_repoll = true;
                    } else {
                        let used = try_turn!(self.session.service_cm_destructions(
                            self.io.core_mut(),
                            self.resources.as_ref(),
                            limit,
                            observed_cm_would_block,
                            &mut actions,
                        ));
                        requires_repoll |= (used == limit || !observed_cm_would_block)
                            && self.session.has_cm_destruction_work();
                    }
                }
                ReactorSource::SessionDeadlineIngress => {
                    let limit = session_deadline_count.min(self.session.reclamation_budget());
                    let used = try_turn!(self.session.service_deadline_requests(limit));
                    requires_repoll |= used == limit && self.session.deadline_request_count() != 0;
                }
                ReactorSource::SessionDeadline => {
                    let limit = session_due_count.min(self.session.reclamation_budget());
                    let used = try_turn!(self.session.service_due_deadlines_into(
                        now,
                        limit,
                        self.io.core_mut(),
                        &mut actions
                    ));
                    requires_repoll |= used == limit && self.session.due_deadline_count(now) != 0;
                }
                ReactorSource::ShutdownPendingOutbound
                | ReactorSource::ShutdownRoutes
                | ReactorSource::ShutdownPendingListen
                | ReactorSource::ShutdownListeners
                | ReactorSource::ShutdownRetainedListeners => {
                    let class = match source {
                        ReactorSource::ShutdownPendingOutbound => CmShutdownClass::PendingOutbound,
                        ReactorSource::ShutdownRoutes => CmShutdownClass::Routes,
                        ReactorSource::ShutdownPendingListen => CmShutdownClass::PendingListen,
                        ReactorSource::ShutdownListeners => CmShutdownClass::Listeners,
                        ReactorSource::ShutdownRetainedListeners => {
                            CmShutdownClass::RetainedListeners
                        }
                        _ => unreachable!("matched one CM shutdown source"),
                    };
                    let limit = shutdown_snapshot
                        .count(class)
                        .min(self.session.cm_budget())
                        .min(actions.remaining() / 8);
                    if limit == 0 {
                        requires_repoll = true;
                    } else {
                        let used =
                            self.session
                                .service_cm_shutdown_class(class, limit, &mut actions);
                        requires_repoll |= used != 0 && self.session.shutdown_work_pending();
                    }
                }
                ReactorSource::ShutdownConnections => {
                    let limit = self.session.cm_budget().min(actions.remaining() / 8);
                    if limit == 0 {
                        requires_repoll = true;
                    } else {
                        let used = self.session.service_shutdown_connections(
                            self.io.core_mut(),
                            limit,
                            &mut actions,
                        );
                        requires_repoll |= used != 0 && self.session.shutdown_work_pending();
                    }
                }
                ReactorSource::IoTerminal => {
                    let budget = self
                        .io
                        .cq_budget()
                        .saturating_add(self.io.completion_dispatch_budget())
                        .saturating_add(self.io.reclamation_budget());
                    let budget = budget.min(actions.remaining());
                    if budget == 0 {
                        requires_repoll = true;
                    } else {
                        let (_, complete) =
                            self.io
                                .service_terminal(budget, &mut self.session, &mut actions);
                        requires_repoll |= !complete;
                    }
                }
            }
        }
        self.session.finish_turn(
            shutting_down,
            terminal_failure,
            observed_cm_would_block,
            self.resources.as_ref(),
        );
        let connection_diagnostics = self.session.connections.admission_snapshot();
        #[cfg(any(test, feature = "test-hooks"))]
        shared.update_cm_rejections(
            self.session
                .manager
                .rejected_cm_events
                .load(std::sync::atomic::Ordering::Acquire),
        );
        #[cfg(any(test, feature = "test-hooks"))]
        shared.update_io_rejections(self.io.core().rejected_cqe_reasons());
        // Sources made ready after the entry snapshot are deliberately not
        // appended to this turn, but they must schedule the next external
        // poll. This includes the common CM-event -> listener-work chain.
        requires_repoll |= self.io.completion_source_count() != 0
            || self.io.reclamation_request_count() != 0
            || self.io.due_deadline_count(Instant::now()) != 0
            || self.session.has_cm_software_work()
            || self.session.has_pending_cm_event()
            || self.session.has_cm_destruction_work()
            || self.session.deadline_request_count() != 0
            || self.session.due_deadline_count(Instant::now()) != 0
            || self.session.shutdown_work_pending()
            || shared
                .commands
                .has_runnable(self.session.cm.listener_slot_available());
        if actions.can_accept(1) {
            self.progress_driver_terminal(shared, &mut actions);
        } else if self.lifecycle.shutdown_is_pending() {
            requires_repoll = true;
        }
        self.publish_diagnostics(shared, connection_diagnostics);
        Ok(ReactorTurn {
            actions,
            requires_repoll,
        })
    }

    pub(super) fn next_deadline(&self) -> Option<tokio::time::Instant> {
        match (self.io.next_deadline(), self.session.next_deadline()) {
            (Some(io), Some(session)) => Some(io.min(session)),
            (Some(io), None) => Some(io),
            (None, Some(session)) => Some(session),
            (None, None) => None,
        }
    }

    pub(super) fn release_resources(&mut self) {
        if let Some(resources) = self.resources.as_mut() {
            resources.drop_readiness_adapters();
        }
        self.resources.take();
    }

    pub(super) fn requires_complete_quarantine(&self) -> bool {
        self.session.connections.live() != 0
            || self.session.cm.retained_session_owner_count() != 0
            || self.io.core().accepted_count() != 0
    }

    fn publish_diagnostics(
        &self,
        shared: &EngineFrontendRoot,
        connection: super::session::connection::ConnectionStateCountSnapshot,
    ) {
        let io = self.io.diagnostics();
        shared.publish_diagnostics(PublishedDiagnostics {
            engine: RdmaEngineDiagnostics {
                lifecycle: self.lifecycle.lifecycle(),
                terminal_error: self
                    .lifecycle
                    .outcome()
                    .and_then(|outcome| outcome.summary()),
                live_connections: connection
                    .live
                    .max(shared.commands.connection_reservations()),
                registered_operations: io.registered_operations,
                accepted_operations: io.accepted_operations,
                pending_reclamations: io.pending_reclamations,
                available_cq_credits: io.available_cq_credits,
                retained_cq_credits: io.retained_cq_credits,
                quarantined_operations: io.quarantined_operations,
                quarantined_mrs: io.quarantined_mrs,
                quarantined_bytes: io.quarantined_bytes,
                quarantined_connections: connection.quarantined_bundles,
            },
            cm_pending_routes: self.session.cm.pending_lifecycle_work_count(),
            cm_retained_owners: self
                .session
                .cm
                .retained_owner_count(&self.session.connections),
        });
    }

    #[cfg(test)]
    pub(super) fn turn_for_test(
        &mut self,
        shared: &Arc<EngineFrontendRoot>,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> super::Result<bool> {
        match self.turn(shared, mode, cx) {
            Ok(turn) => {
                let requires_repoll = turn.requires_repoll;
                self.last_action_count = turn.actions.len();
                turn.actions.publish();
                Ok(requires_repoll)
            }

            Err(failure) => {
                self.last_action_count = failure.actions.len();
                failure.actions.publish();
                Err(failure.error)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn take_served_sources_for_test(&mut self) -> Vec<String> {
        std::mem::take(&mut self.served_sources)
            .into_iter()
            .map(|source| format!("{source:?}"))
            .collect()
    }

    #[cfg(test)]
    pub(super) fn last_action_count_for_test(&self) -> usize {
        self.last_action_count
    }

    /// Synchronous fail-closed termination when no later poll can occur.
    pub(super) fn terminate_on_driver_drop(
        &mut self,
        shared: &Arc<EngineFrontendRoot>,
    ) -> ReactorActions {
        let actions = self.handle_driver_drop(shared);
        self.publish_diagnostics(shared, self.session.connections.admission_snapshot());
        #[cfg(any(test, feature = "test-hooks"))]
        shared.update_cm_rejections(
            self.session
                .manager
                .rejected_cm_events
                .load(std::sync::atomic::Ordering::Acquire),
        );
        // A terminal outcome can intentionally publish diagnostics with
        // retained connection bundles excluded. Do not use that copied
        // snapshot as the ownership proof for dropping the reactor: setup
        // rollback and destruction quarantines still live in the exclusive
        // registry even when no operation debt remains.
        let retain_reactor = self.requires_complete_quarantine();
        if retain_reactor {
            let replacement_manager = SessionManager::from_frontend(
                Arc::clone(&shared.session),
                Arc::downgrade(&shared.control),
                #[cfg(any(test, feature = "test-hooks"))]
                super::SessionTestInstrumentation {
                    driver: Arc::clone(&shared.test_driver),
                },
            );
            let retained =
                std::mem::replace(self, EngineReactor::new(shared, replacement_manager, None));
            failed_reactor_quarantine()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(RetainedEngineReactor {
                    _reactor: retained,
                    _shared: Arc::clone(shared),
                });
        }
        actions
    }
}

struct RetainedEngineReactor {
    _reactor: EngineReactor,
    _shared: Arc<EngineFrontendRoot>,
}

fn failed_reactor_quarantine() -> &'static Mutex<Vec<RetainedEngineReactor>> {
    static REACTORS: OnceLock<Mutex<Vec<RetainedEngineReactor>>> = OnceLock::new();
    REACTORS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
pub(in crate::v2::engine) fn failed_reactor_contains(shared: &Arc<EngineFrontendRoot>) -> bool {
    failed_reactor_quarantine()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .any(|retained| Arc::ptr_eq(&retained._shared, shared))
}
