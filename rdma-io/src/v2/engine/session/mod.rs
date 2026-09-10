//! Connection/session ownership for the explicitly driven v2 engine.
//!
//! The driver-owned [`super::reactor::EngineReactor`] owns connection and
//! listener identity, the shared CM dispatcher/destruction service, admission,
//! deadlines, retirement, shutdown, and quarantine. `SessionContext` is a
//! runtime-state-free policy and frontend capability; mutable session state
//! remains in [`SessionReactorSources`] and connection-owned records.

#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

pub(in crate::v2::engine) use self::cm::{
    CmShutdownClass, CmShutdownSnapshot, CmSoftwareClass, CmSoftwareSnapshot,
};
pub(super) mod cm;
pub(super) mod connection;
mod drain;
pub(super) mod listener;
mod progress;
pub(in crate::v2::engine) mod registry;

use self::listener::{ListenerAdmission, ListenerEntry};
pub(super) use self::progress::SessionReactorSources;
use self::registry::ConnectionRegistry;
#[cfg(any(test, feature = "test-hooks"))]
use super::SessionTestInstrumentation;
use super::config::{ProviderLimits, RdmaConnectionConfig, SessionConfig};
use super::io::MemoryRegistrar;
#[cfg(test)]
use super::io_core::IoState;
use super::io_core::{CommittedIoCoreEffects, IoCoreEffects, OperationQuarantineEffect};
use super::reactor::CommandIngress;
#[cfg(test)]
use super::registry::OperationToken;
use super::registry::{ConnectionToken, ListenerToken, Lookup, lock_unpoison};
use super::{EngineControl, EngineObserver, Result};
use crate::v2::error::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeadlineKind {
    ConnectionDrain,
    EngineShutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DeadlineRequest {
    at: tokio::time::Instant,
    kind: DeadlineKind,
    token: u64,
}

/// Exact, non-cloneable proof minted after one successful synchronous QP destroy.
pub(super) struct QpDestructionProof {
    connection: ConnectionToken,
    qp_num: u32,
    _evidence: (),
}

/// Resource-free close observation shared with connection frontends.
pub(super) struct SessionCloseState {
    pub(super) outcome: Mutex<Option<super::lifecycle::MemoizedTerminalResult>>,
    engine_terminal: Mutex<Option<super::lifecycle::MemoizedTerminalResult>>,
    pub(super) notify: Arc<tokio::sync::Notify>,
    retired: AtomicBool,
}

impl SessionCloseState {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(None),
            engine_terminal: Mutex::new(None),
            notify: Arc::new(tokio::sync::Notify::new()),
            retired: AtomicBool::new(false),
        })
    }

    pub(super) fn notify(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    pub(super) fn outcome(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        let outcome = self.raw_outcome();
        match outcome {
            Some(ref value) if value.is_connection_quarantined() => outcome,
            Some(_) if self.is_retired() => outcome,
            _ => lock_unpoison(&self.engine_terminal).clone(),
        }
    }

    pub(super) fn raw_outcome(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        lock_unpoison(&self.outcome).clone()
    }

    pub(super) fn mark_retired(&self) {
        self.retired
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn record_engine_terminal(
        &self,
        outcome: &super::lifecycle::MemoizedTerminalResult,
    ) {
        let mut terminal = lock_unpoison(&self.engine_terminal);
        if terminal.is_none() {
            *terminal = Some(outcome.clone());
        }
    }

    pub(super) fn is_retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn notify_waiters(self: &Arc<Self>) {
        self.notify.notify_waiters();
    }

    pub(super) fn notify_waiters_into(
        self: &Arc<Self>,
        actions: &mut super::reactor::ReactorActions,
    ) {
        let close = Arc::clone(self);
        actions.push_close_or_listener(move || close.notify.notify_waiters());
    }
}

/// Resource-free close observation for an engine-owned listener.
pub(super) struct SessionListenerCloseState {
    outcome: Mutex<Option<super::lifecycle::MemoizedTerminalResult>>,
    notify: tokio::sync::Notify,
    frontend_count: AtomicUsize,
}

impl SessionListenerCloseState {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(None),
            notify: tokio::sync::Notify::new(),
            frontend_count: AtomicUsize::new(1),
        })
    }

    pub(super) fn outcome(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        lock_unpoison(&self.outcome).clone()
    }

    pub(super) fn store_if_empty(&self, outcome: super::lifecycle::MemoizedTerminalResult) -> bool {
        let mut current = lock_unpoison(&self.outcome);
        if current.is_none() {
            *current = Some(outcome);
            true
        } else {
            false
        }
    }

    pub(super) fn retain_frontend(&self) {
        self.frontend_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn release_frontend(&self) -> bool {
        let previous = self.frontend_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "listener frontend count must be positive");
        previous == 1
    }

    pub(super) fn notify_waiters_into(
        self: &Arc<Self>,
        actions: &mut super::reactor::ReactorActions,
    ) {
        let close = Arc::clone(self);
        actions.push_close_or_listener(move || close.notify.notify_waiters());
    }
}

/// Narrow frontend policy and observation endpoint.
///
/// This value contains no connection, listener, CM, shutdown, terminal, or
/// provider-progress owner. Public handles use it only for immutable
/// validation/registration capability, typed command routing, and terminal
/// observation.
pub(super) struct SessionFrontend {
    pub(super) admission: Arc<RwLock<()>>,
    pub(super) self_ref: Weak<SessionFrontend>,
    pub(super) commands: Weak<CommandIngress>,
    observer: Weak<EngineObserver>,
    work_signal: Weak<super::driver::WorkSignal>,
    config: SessionConfig,
    provider: Option<ProviderLimits>,
    memory: MemoryRegistrar,
    #[cfg(any(test, feature = "test-hooks"))]
    test_instrumentation: SessionTestInstrumentation,
}

impl SessionFrontend {
    fn new(
        config: SessionConfig,
        provider: Option<ProviderLimits>,
        admission: Arc<RwLock<()>>,
        memory: MemoryRegistrar,
        commands: &Arc<CommandIngress>,
        observer: &Arc<EngineObserver>,
        work_signal: &Arc<super::driver::WorkSignal>,
        #[cfg(any(test, feature = "test-hooks"))] test_instrumentation: SessionTestInstrumentation,
    ) -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            admission,
            self_ref: self_ref.clone(),
            commands: Arc::downgrade(commands),
            observer: Arc::downgrade(observer),
            work_signal: Arc::downgrade(work_signal),
            config,
            provider,
            memory,
            #[cfg(any(test, feature = "test-hooks"))]
            test_instrumentation,
        })
    }

    pub(super) fn admission_error(&self) -> Option<Error> {
        if let Some(outcome) = self
            .observer
            .upgrade()
            .and_then(|observer| observer.outcome())
        {
            return outcome.into_result().err();
        }
        self.commands
            .upgrade()
            .and_then(|commands| commands.admission_error())
            .or_else(|| {
                self.commands
                    .upgrade()
                    .is_none()
                    .then_some(Error::DriverShutdown)
            })
    }

    pub(super) fn engine_outcome(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        self.observer
            .upgrade()
            .and_then(|observer| observer.outcome())
    }

    pub(super) fn validate_connection_config(&self, config: &RdmaConnectionConfig) -> Result<()> {
        config.validate(&self.config, self.provider.as_ref())
    }

    pub(super) fn memory_registrar(&self) -> MemoryRegistrar {
        self.memory.clone()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn provider_limits(&self) -> Option<ProviderLimits> {
        self.provider
    }

    pub(super) fn notify_reactor(&self) {
        if let Some(work_signal) = self.work_signal.upgrade() {
            work_signal.notify_reactor();
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn pause_connect_before_enqueue(&self) {
        self.test_instrumentation.pause_connect_before_enqueue();
    }
}

/// Opaque request/observation capability for a reactor-owned listener.
#[derive(Clone)]
pub(super) struct SessionListener {
    frontend: Weak<SessionFrontend>,
    commands: Weak<CommandIngress>,
    token: ListenerToken,
    admission: Arc<ListenerAdmission>,
    close: Arc<SessionListenerCloseState>,
    local_addr: std::net::SocketAddr,
}

type SessionListenerOwners = (
    Arc<SessionFrontend>,
    Arc<CommandIngress>,
    ListenerToken,
    Arc<ListenerAdmission>,
);

impl SessionListener {
    pub(super) fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    pub(super) fn retain_frontend(&self) {
        self.close.retain_frontend();
    }

    pub(super) fn release_frontend(&self) -> bool {
        self.close.release_frontend()
    }

    pub(super) fn owners(&self) -> Result<SessionListenerOwners> {
        let frontend = self.frontend.upgrade().ok_or(Error::DriverShutdown)?;
        let commands = self.commands.upgrade().ok_or_else(|| {
            self.close
                .outcome()
                .and_then(|outcome| outcome.into_result().err())
                .unwrap_or(Error::DriverShutdown)
        })?;
        Ok((frontend, commands, self.token, Arc::clone(&self.admission)))
    }

    pub(super) fn request_close(&self) {
        if self.close.outcome().is_some() {
            return;
        }
        let Ok((frontend, commands, token, admission)) = self.owners() else {
            return;
        };
        commands.request_listener_close(&frontend, token, &admission);
    }

    pub(super) async fn close(&self) -> Result<()> {
        self.request_close();
        loop {
            let notified = self.close.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.close.outcome() {
                return outcome.into_result();
            }
            if let Some(frontend) = self.frontend.upgrade()
                && let Some(outcome) = frontend.engine_outcome()
            {
                return outcome.into_result();
            }
            notified.await;
        }
    }
}

/// Runtime-state-free backend policy used by the reactor.
///
/// All mutable connection, listener, CM, shutdown, and terminal storage is
/// owned by the reactor. Public handles cannot reach this value; they retain
/// only [`SessionFrontend`], typed command ingress, and take-once observers.
pub(super) struct SessionContext {
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cm_events: AtomicU64,
    #[cfg(test)]
    pub(super) shutdown_connection_close_started: AtomicBool,
    frontend: Arc<SessionFrontend>,
    control: Weak<EngineControl>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_instrumentation: SessionTestInstrumentation,
}

fn apply_io_effects(
    context: &SessionContext,
    connections: &mut ConnectionRegistry,
    mut effects: IoCoreEffects,
) -> CommittedIoCoreEffects {
    for effect in effects.take_quarantine() {
        match effect {
            OperationQuarantineEffect::Added {
                connection,
                operation,
            } => {
                connections.track_operation_quarantine(connection, operation);
            }
            OperationQuarantineEffect::Cleared {
                connection,
                operation,
            } => {
                connections.clear_operation_quarantine(connection, operation);
            }
        }
    }
    for token in effects.take_drained() {
        if connections.close_started(token) {
            connections.clear_bundle_quarantine(token);
            SessionReactorSources::record_connection_drained(connections, token);
            SessionReactorSources::schedule_connection_retirement(context, connections, token);
        }
    }
    effects.into_committed()
}

impl SessionContext {
    pub(super) fn new(
        config: SessionConfig,
        provider: Option<ProviderLimits>,
        admission: Arc<RwLock<()>>,
        memory: MemoryRegistrar,
        control: Weak<EngineControl>,
        commands: &Arc<CommandIngress>,
        observer: &Arc<EngineObserver>,
        work_signal: &Arc<super::driver::WorkSignal>,
        #[cfg(any(test, feature = "test-hooks"))] test_instrumentation: SessionTestInstrumentation,
    ) -> Result<Self> {
        let frontend = SessionFrontend::new(
            config,
            provider,
            Arc::clone(&admission),
            memory,
            commands,
            observer,
            work_signal,
            #[cfg(any(test, feature = "test-hooks"))]
            test_instrumentation.clone(),
        );
        Ok(Self::from_frontend(
            frontend,
            control,
            #[cfg(any(test, feature = "test-hooks"))]
            test_instrumentation,
        ))
    }

    pub(super) fn from_frontend(
        frontend: Arc<SessionFrontend>,
        control: Weak<EngineControl>,
        #[cfg(any(test, feature = "test-hooks"))] test_instrumentation: SessionTestInstrumentation,
    ) -> Self {
        Self {
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cm_events: AtomicU64::new(0),
            #[cfg(test)]
            shutdown_connection_close_started: AtomicBool::new(false),
            frontend,
            control,
            #[cfg(any(test, feature = "test-hooks"))]
            test_instrumentation,
        }
    }

    pub(super) fn frontend(&self) -> Arc<SessionFrontend> {
        Arc::clone(&self.frontend)
    }

    pub(super) fn max_live_connections(&self) -> usize {
        self.frontend.config.max_live_connections
    }

    pub(super) fn connection_drain_deadline(&self) -> std::time::Duration {
        self.frontend.config.connection_drain_deadline
    }

    pub(super) fn admission_error(&self) -> Option<Error> {
        self.frontend.admission_error()
    }

    pub(super) fn shutdown_requested(&self) -> bool {
        self.frontend
            .commands
            .upgrade()
            .is_none_or(|commands| commands.is_closed())
    }

    pub(super) fn begin_driver_failure(&self, error: Error) {
        if let Some(control) = self.control.upgrade() {
            control.request_driver_failure(error);
        }
    }

    pub(super) fn notify_reactor(&self) {
        if let Some(control) = self.control.upgrade() {
            control.notify_reactor();
        }
    }

    pub(super) fn validate_connection_config(&self, config: &RdmaConnectionConfig) -> Result<()> {
        self.frontend.validate_connection_config(config)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn take_setup_rollback_failure(
        &self,
    ) -> Option<super::driver::test_api::SetupRollbackFailure> {
        self.test_instrumentation.take_setup_rollback_failure()
    }

    pub(super) fn listener_capability(&self, listener: &ListenerEntry) -> SessionListener {
        let admission = listener.admission();
        debug_assert_eq!(admission.token(), listener.token);
        SessionListener {
            frontend: self.frontend.self_ref.clone(),
            commands: self.frontend.commands.clone(),
            token: listener.token,
            admission,
            close: listener.close_state(),
            local_addr: listener
                .local_addr
                .expect("published listener has a local address"),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn transition_connection_to_error_token(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> Result<()> {
        connections
            .transition_connection_to_error(token)
            .map(|_| ())
    }

    #[cfg(test)]
    pub(super) fn reclaim_after_qp_destroy(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut IoState,
        proof: QpDestructionProof,
        connection: ConnectionToken,
        tokens: Vec<OperationToken>,
    ) -> usize {
        let QpDestructionProof {
            connection: proven_connection,
            qp_num: proven_qp_num,
            _evidence: (),
        } = proof;
        let Some((qp_num, close_error)) = connections.with_connection(connection, |connection| {
            (connection.qp_num(), connection.operation_close_error())
        }) else {
            return 0;
        };
        if proven_connection != connection || proven_qp_num != qp_num {
            tracing::warn!(
                connection = connection.encode(),
                "operation reclaim rejected a mismatched QP destruction proof"
            );
            return 0;
        }
        tokens
            .into_iter()
            .filter(|token| {
                self.reclaim_after_proven_qp_destroy(
                    connections,
                    io_core,
                    proven_connection,
                    proven_qp_num,
                    connection,
                    close_error.clone(),
                    *token,
                )
            })
            .count()
    }

    #[cfg(test)]
    #[allow(
        clippy::too_many_arguments,
        reason = "proof validation fixture keeps every expected identity and owner explicit"
    )]
    fn reclaim_after_proven_qp_destroy(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut IoState,
        proven_connection: ConnectionToken,
        proven_qp_num: u32,
        connection: ConnectionToken,
        close_error: Error,
        token: OperationToken,
    ) -> bool {
        let Some((reclaimed, effects)) =
            connections.with_connection_io_mut(connection, |io, connection_io, _poster| {
                io_core.reclaim_after_qp_destroy(
                    proven_connection,
                    proven_qp_num,
                    io,
                    connection_io,
                    close_error,
                    token,
                )
            })
        else {
            return false;
        };
        apply_io_effects(self, connections, effects).publish();
        reclaimed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{CompletionMode, RdmaConnectionConfig, test_engine_pair};
    use super::DeadlineKind;
    use super::connection::{TestConnectionProvider, install_connection};
    use super::listener::{ListenerEntry, RdmaListener};
    use crate::v2::error::{Error, Result};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct TestPoster {
        qp_num: u32,
    }

    impl TestConnectionProvider for TestPoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn post_recv(&self, _: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn to_error(&self) -> Result<()> {
            Ok(())
        }

        fn destroy_qp(&self) -> Result<bool> {
            Ok(true)
        }

        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reactor_owns_registry_admission_and_deadline_inbox() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connections = &mut driver.reactor.session.connections;

        assert_eq!(connections.live(), 0);
        assert_eq!(connections.admission_snapshot().live, 0);
        assert_eq!(connections.deadline_request_count(), 0);

        connections.schedule_deadline(DeadlineKind::ConnectionDrain, 7, std::time::Duration::ZERO);
        let requests = connections.take_deadline_requests(1);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].kind, DeadlineKind::ConnectionDrain);
        assert_eq!(requests[0].token, 7);
        assert_eq!(connections.deadline_request_count(), 0);
    }

    #[test]
    fn connection_handle_routes_close_by_identity() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let frontend_owners = Arc::strong_count(&engine.shared.session);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::new(TestPoster { qp_num: 17 }),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .expect("install synthetic connection");
        assert_eq!(
            Arc::strong_count(&engine.shared.session),
            frontend_owners,
            "public connection capability retains only SessionFrontend"
        );
        let token = connection.session_token();
        connection.request_close();
        assert!(!driver.reactor.session.connections.close_started(token));
        engine.shared.commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert!(driver.reactor.session.connections.close_started(token));
        assert!(
            driver
                .reactor
                .session
                .connections
                .with_connection(token, |connection| connection.error_transition_complete())
                .unwrap()
        );
    }

    #[test]
    fn connection_close_observer_waits_for_retirement_after_cm_failure() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::new(TestPoster { qp_num: 18 }),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .expect("install synthetic connection");
        let token = connection.session_token();
        let _pending = driver
            .reactor
            .session
            .connections
            .with_connection_mut(token, |connection| {
                connection.mark_cm_failure(Error::TransportClosed)
            })
            .flatten();
        assert!(
            connection.close_state().outcome().is_none(),
            "ordinary close errors remain hidden until QP/CmId retirement"
        );
        let _pending = driver
            .reactor
            .session
            .connections
            .with_connection_mut(token, |connection| connection.finish_retirement())
            .flatten();
        assert!(matches!(
            connection.close_state().outcome().unwrap().into_result(),
            Err(Error::TransportClosed)
        ));
    }

    #[test]
    fn session_listener_capability_is_resource_free() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let state = ListenerEntry::test_only(4);
        let frontend_owners = Arc::strong_count(&engine.shared.session);
        let listener = RdmaListener::from_state(&driver.reactor.session.manager, &state);

        assert_eq!(listener.local_addr().unwrap(), state.local_addr.unwrap());
        assert_eq!(
            Arc::strong_count(&engine.shared.session),
            frontend_owners,
            "public listener capability retains only SessionFrontend"
        );
        let clone = listener.clone();
        assert_eq!(Arc::strong_count(&engine.shared.session), frontend_owners);
        drop(clone);
        drop(listener);
        assert!(
            engine.shared.commands.has_pending(),
            "last frontend routes close only by stable listener identity"
        );
    }

    #[test]
    fn qp_destroy_mints_one_exact_non_replayable_proof() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Arc::new(TestPoster { qp_num: 19 }),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .expect("install synthetic connection");
        let token = connection.session_token();
        let proof = driver
            .reactor
            .session
            .connections
            .establish_qp_destruction_proof(token)
            .expect("first successful destroy mints proof");
        assert_eq!(proof.connection, token);
        assert_eq!(proof.qp_num, connection.identity().qp_num());
        assert!(matches!(
            driver
                .reactor
                .session
                .connections
                .establish_qp_destruction_proof(token),
            Err(Error::InvalidConfig(message)) if message.contains("cannot be replayed")
        ));

        assert_eq!(
            driver.reactor.session.manager.reclaim_after_qp_destroy(
                &mut driver.reactor.session.connections,
                driver.reactor.io.core_mut(),
                proof,
                token,
                Vec::new(),
            ),
            0
        );
    }
}
