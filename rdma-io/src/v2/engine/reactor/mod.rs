//! Driver-owned command, I/O, connection-lifecycle, and session progress.
//!
//! Connection identity, routes, admission, deadlines, retirement, and
//! quarantine live under this reactor. The shared session service retains the
//! single listener/context-route and CM-destruction adapters until listener
//! lifecycle moves as one unit.

mod action;
pub(super) mod command;
pub(super) mod completion;
mod scheduler;

use std::sync::{Arc, Mutex, OnceLock};
use std::task::Context as TaskContext;
use tokio::time::Instant;

use super::EngineShared;
use super::config::CompletionMode;
use super::io_core::{IoDriverSignal, IoReactorSources, IoState};
use super::progress::ReadinessRegistration;
use super::resources::EngineResources;
use super::session::{CmShutdownClass, CmSoftwareClass, SessionReactorSources};

#[cfg(test)]
pub(super) use action::REACTOR_ACTION_BUDGET;
pub(super) use action::{DeferredProtocolActions, ReactorActions};
pub(super) use command::CommandIngress;
use scheduler::{ReactorScheduler, ReactorSource};

/// Driver-owned scheduling state for one bounded top-level reactor turn.
///
/// Listener identity and common CM adapter state remain in `SessionManager`;
/// all connection-only mutable state is owned through `session`.
pub(super) struct EngineReactor {
    pub(super) io: IoReactorSources,
    pub(super) session: SessionReactorSources,
    scheduler: ReactorScheduler,
}

pub(super) struct ReactorTurn {
    pub(super) actions: ReactorActions,
    pub(super) requires_repoll: bool,
}

pub(super) struct ReactorTurnFailure {
    pub(super) error: super::Error,
    pub(super) actions: ReactorActions,
}

#[cfg(test)]
struct ReactorTestBridge;

#[cfg(test)]
impl super::io_core::IoSessionBridge for ReactorTestBridge {
    fn route_completion(
        &self,
        _io: &mut IoState,
        _completion: crate::wc::WorkCompletion,
    ) -> Option<super::registry::ConnectionToken> {
        None
    }

    fn dispatch_connection_completions(
        &self,
        _io: &mut IoState,
        _connection: super::registry::ConnectionToken,
        _quantum: usize,
    ) -> (usize, bool) {
        (0, false)
    }

    fn handle_reclamation_deadline(
        &self,
        _io: &mut IoState,
        _token: super::registry::OperationToken,
    ) {
    }

    fn commit_terminal_effects(&self, _effects: super::io_core::IoCoreEffects) {}
}

impl EngineReactor {
    pub(super) fn new(shared: &Arc<EngineShared>, resources: Option<EngineResources>) -> Self {
        let (io_resources, session_resources) = match resources {
            Some(mut resources) => {
                let io = resources.take_io_progress_resources();
                (Some(io), Some(resources.into_session_progress()))
            }
            None => (None, None),
        };
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
        #[cfg(test)]
        let bridge: Arc<dyn super::io_core::IoSessionBridge> = Arc::new(ReactorTestBridge);
        Self {
            io: IoReactorSources::new(
                io_core,
                #[cfg(test)]
                bridge,
                io_resources,
                shared.config.cq_completion_budget,
                shared.config.completion_dispatch_budget,
                shared.config.io_reclamation_budget,
                #[cfg(any(test, feature = "test-hooks"))]
                Arc::clone(&shared.test_driver),
            ),
            session: SessionReactorSources::new(
                Arc::clone(&shared.session),
                shared.commands.connection_admission(),
                session_resources,
                shared.config.cm_event_budget,
                shared.config.session_reclamation_budget,
                shared.config.shutdown_deadline,
            ),
            scheduler: ReactorScheduler::new(),
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "failure preserves the already-bounded action batch for publication"
    )]
    pub(super) fn turn(
        &mut self,
        shared: &Arc<EngineShared>,
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
        self.io
            .sync_lifecycle(shared.admission_error(), shared.pending_terminal_outcome());
        let io_terminal = try_turn!(self.io.begin_turn());
        let (shutting_down, terminal_failure) = try_turn!(self.session.begin_turn());
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
        let commands_ready = shared.commands.has_pending();

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
                !terminal_failure && cm_software_snapshot.count(CmSoftwareClass::ListenerWork) != 0,
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
        let mut observed_cm_would_block = self.session.resources_absent();
        while let Some(source) = ready.pop_front() {
            match source {
                ReactorSource::Commands => {
                    let report = shared.commands.service_turn_into(
                        shared,
                        self.io.core_mut(),
                        &mut self.session,
                        &mut actions,
                    );
                    if report.has_more {
                        shared.work_signal.publish(super::driver::REACTOR_WORK);
                    }
                    requires_repoll |= report.has_more || report.session_work;
                }
                ReactorSource::Cq => {
                    let (_, _, repoll) = try_turn!(self.io.service_cq(&mut self.session, mode, cx));
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
        self.session
            .finish_turn(shutting_down, terminal_failure, observed_cm_would_block);
        shared.update_io_diagnostics(self.io.diagnostics());
        let diagnostics = if terminal_failure {
            self.session
                .connections
                .admission_snapshot_excluding_retained()
        } else {
            self.session.connections.admission_snapshot()
        };
        shared.update_connection_diagnostics(diagnostics);
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
            || shared.commands.has_pending();
        if actions.can_accept(1) {
            shared.progress_driver_terminal(&self.io, &self.session, &mut actions);
        } else if shared.shutdown_is_pending() {
            requires_repoll = true;
        }
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
        self.io.release_resources();
        self.session.release_resources();
    }

    #[cfg(test)]
    pub(super) fn session_turn_for_test(
        &mut self,
        mode: CompletionMode,
        cx: &mut TaskContext<'_>,
    ) -> super::Result<super::progress::ProgressReport> {
        self.session.turn(self.io.core_mut(), mode, cx)
    }

    /// Synchronous fail-closed termination when no later poll can occur.
    pub(super) fn terminate_on_driver_drop(
        &mut self,
        shared: &Arc<EngineShared>,
    ) -> ReactorActions {
        let actions = shared.handle_driver_drop(&mut self.session, self.io.core_mut());
        shared.update_connection_diagnostics(self.session.connections.admission_snapshot());
        // A terminal outcome can intentionally publish diagnostics with
        // retained connection bundles excluded. Do not use that copied
        // snapshot as the ownership proof for dropping the reactor: setup
        // rollback and destruction quarantines still live in the exclusive
        // registry even when no operation debt remains.
        let retain_reactor = shared
            .failure_retained
            .load(std::sync::atomic::Ordering::Acquire)
            || self.session.connections.live() != 0
            || self.io.core().accepted_count() != 0;
        if retain_reactor {
            let retained = std::mem::replace(self, EngineReactor::new(shared, None));
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
    _shared: Arc<EngineShared>,
}

fn failed_reactor_quarantine() -> &'static Mutex<Vec<RetainedEngineReactor>> {
    static REACTORS: OnceLock<Mutex<Vec<RetainedEngineReactor>>> = OnceLock::new();
    REACTORS.get_or_init(|| Mutex::new(Vec::new()))
}
