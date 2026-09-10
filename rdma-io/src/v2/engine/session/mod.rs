//! Connection/session ownership for the explicitly driven v2 engine.
//!
//! The driver-owned [`super::reactor::EngineReactor`] owns connection and
//! listener identity, the shared CM dispatcher/destruction service, admission,
//! deadlines, retirement, shutdown, and quarantine. `SessionManager` is now a
//! runtime-state-free policy/observer capability only; it owns no listener, route,
//! connection, or terminal lifecycle storage.

#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

pub(in crate::v2::engine) use self::cm::{
    CmShutdownClass, CmShutdownSnapshot, CmSoftwareClass, CmSoftwareSnapshot,
};
pub(super) mod cm;
pub(super) mod connection;
mod drain;
pub(super) mod listener;
mod progress;
pub(in crate::v2::engine) mod registry;

use self::connection::{QpDestroyStatus, SharedCmId};
use self::listener::{ListenerAdmission, ListenerEntry};
pub(super) use self::progress::SessionReactorSources;
use self::registry::ConnectionRegistry;
#[cfg(any(test, feature = "test-hooks"))]
use super::SessionTestInstrumentation;
use super::config::{ProviderLimits, RdmaConnectionConfig, SessionConfig};
use super::io::MemoryRegistrar;
use super::io_core::{CommittedIoCoreEffects, IoCoreEffects, IoState, OperationQuarantineEffect};
use super::reactor::CommandIngress;
use super::registry::{ConnectionToken, ListenerToken, Lookup, OperationToken, lock_unpoison};
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
    self_ref: OnceLock<Weak<SessionFrontend>>,
    commands: OnceLock<Weak<CommandIngress>>,
    observer: OnceLock<Weak<EngineObserver>>,
    work_signal: OnceLock<Weak<super::driver::WorkSignal>>,
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
        #[cfg(any(test, feature = "test-hooks"))] test_instrumentation: SessionTestInstrumentation,
    ) -> Arc<Self> {
        Arc::new(Self {
            admission,
            self_ref: OnceLock::new(),
            commands: OnceLock::new(),
            observer: OnceLock::new(),
            work_signal: OnceLock::new(),
            config,
            provider,
            memory,
            #[cfg(any(test, feature = "test-hooks"))]
            test_instrumentation,
        })
    }

    fn bind_self(self: &Arc<Self>) {
        self.self_ref
            .set(Arc::downgrade(self))
            .unwrap_or_else(|_| panic!("SessionFrontend self reference is bound exactly once"));
    }

    fn bind_commands(&self, commands: &Arc<CommandIngress>) {
        self.commands
            .set(Arc::downgrade(commands))
            .unwrap_or_else(|_| panic!("SessionFrontend command ingress is bound exactly once"));
    }

    fn bind_engine(
        &self,
        observer: &Arc<EngineObserver>,
        work_signal: &Arc<super::driver::WorkSignal>,
    ) {
        if self.observer.set(Arc::downgrade(observer)).is_err()
            || self.work_signal.set(Arc::downgrade(work_signal)).is_err()
        {
            panic!("SessionFrontend is bound to exactly one engine observer and work signal");
        }
    }

    pub(super) fn admission_error(&self) -> Option<Error> {
        if let Some(outcome) = self
            .observer
            .get()
            .and_then(Weak::upgrade)
            .and_then(|observer| observer.outcome())
        {
            return outcome.into_result().err();
        }
        self.commands
            .get()
            .and_then(Weak::upgrade)
            .and_then(|commands| commands.admission_error())
            .or_else(|| {
                self.commands
                    .get()
                    .and_then(Weak::upgrade)
                    .is_none()
                    .then_some(Error::DriverShutdown)
            })
    }

    pub(super) fn engine_outcome(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        self.observer
            .get()
            .and_then(Weak::upgrade)
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

    pub(super) fn publish_session_work(&self) {
        if let Some(work_signal) = self.work_signal.get().and_then(Weak::upgrade) {
            work_signal.publish(super::driver::SESSION_WORK);
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
pub(super) struct SessionManager {
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cm_events: AtomicU64,
    #[cfg(test)]
    pub(super) shutdown_connection_close_started: AtomicBool,
    frontend: Arc<SessionFrontend>,
    control: Weak<EngineControl>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_instrumentation: SessionTestInstrumentation,
}

impl SessionManager {
    pub(super) fn new(
        config: SessionConfig,
        provider: Option<ProviderLimits>,
        admission: Arc<RwLock<()>>,
        memory: MemoryRegistrar,
        control: Weak<EngineControl>,
        #[cfg(any(test, feature = "test-hooks"))] test_instrumentation: SessionTestInstrumentation,
    ) -> Result<Self> {
        let frontend = SessionFrontend::new(
            config,
            provider,
            Arc::clone(&admission),
            memory,
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

    pub(super) fn bind_self(&self) {
        self.frontend.bind_self();
    }

    pub(super) fn bind_commands(&self, commands: &Arc<CommandIngress>) {
        self.frontend.bind_commands(commands);
    }

    pub(super) fn bind_engine(
        &self,
        observer: &Arc<EngineObserver>,
        work_signal: &Arc<super::driver::WorkSignal>,
    ) {
        self.frontend.bind_engine(observer, work_signal);
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
            .get()
            .and_then(Weak::upgrade)
            .is_none_or(|commands| commands.is_closed())
    }

    pub(super) fn begin_driver_failure(&self, error: Error) {
        if let Some(control) = self.control.upgrade() {
            control.request_driver_failure(error);
        }
    }

    pub(super) fn publish_io_work(&self) {
        if let Some(control) = self.control.upgrade() {
            control.publish(super::driver::IO_WORK);
        }
    }

    pub(super) fn publish_session_work(&self) {
        if let Some(control) = self.control.upgrade() {
            control.publish(super::driver::SESSION_WORK);
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

    pub(in crate::v2::engine) fn request_connection_close_into(
        &self,
        cm: &mut cm::CmState,
        connections: &mut ConnectionRegistry,
        io_core: &mut IoState,
        token: ConnectionToken,
        actions: &mut super::reactor::ReactorActions,
    ) {
        let Lookup::Occupied(_) = connections.lookup(token) else {
            return;
        };
        self.begin_connection_close_into(cm, connections, token, io_core, actions);
    }

    pub(super) fn listener_capability(&self, listener: &ListenerEntry) -> SessionListener {
        let admission = listener.admission();
        debug_assert_eq!(admission.token(), listener.token);
        SessionListener {
            frontend: self
                .frontend
                .self_ref
                .get()
                .expect("SessionFrontend self reference is bound before use")
                .clone(),
            commands: self
                .frontend
                .commands
                .get()
                .expect("SessionFrontend command ingress is bound before use")
                .clone(),
            token: listener.token,
            admission,
            close: listener.close_state(),
            local_addr: listener
                .local_addr
                .expect("published listener has a local address"),
        }
    }

    pub(super) fn establish_qp_destruction_proof(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> Result<QpDestructionProof> {
        let status = connections
            .with_connection_mut(token, |connection| connection.destroy_qp_for_session())
            .ok_or(Error::TransportClosed)??;
        match status {
            QpDestroyStatus::DestroyedNow => Ok(QpDestructionProof {
                connection: token,
                qp_num: connections
                    .with_connection(token, |connection| connection.qp_num())
                    .ok_or(Error::TransportClosed)?,
                _evidence: (),
            }),
            QpDestroyStatus::AlreadyDestroyed => Err(Error::InvalidConfig(
                "QP destruction proof was already minted and cannot be replayed".into(),
            )),
        }
    }

    pub(super) fn ensure_qp_destroyed(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> Result<()> {
        match connections
            .with_connection_mut(token, |connection| connection.destroy_qp_for_session())
            .ok_or(Error::TransportClosed)??
        {
            QpDestroyStatus::DestroyedNow | QpDestroyStatus::AlreadyDestroyed => Ok(()),
        }
    }

    pub(super) fn transition_connection_to_error(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> Result<bool> {
        connections
            .with_connection_mut(token, |connection| connection.transition_to_error_once())
            .ok_or(Error::TransportClosed)?
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn transition_connection_to_error_token(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> Result<()> {
        self.transition_connection_to_error(connections, token)
            .map(|_| ())
    }

    pub(super) fn finalize_connection_engine(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
        outcome: &super::lifecycle::MemoizedTerminalResult,
    ) -> Option<super::io::PendingIoEvent> {
        connections
            .with_connection_mut(token, |connection| {
                connection.close_state().record_engine_terminal(outcome);
                connection.finalize_engine(outcome)
            })
            .flatten()
    }

    pub(super) fn finalize_quarantined_connection_engine(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
        outcome: &super::lifecycle::MemoizedTerminalResult,
    ) -> Option<super::io::PendingIoEvent> {
        connections
            .with_connection_mut(token, |connection| {
                connection.close_state().record_engine_terminal(outcome);
                connection.finalize_engine_without_provider(outcome)
            })
            .flatten()
    }

    pub(super) fn destroy_connection_resources(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
        outstanding_operations: usize,
    ) -> Result<Option<SharedCmId>> {
        connections
            .with_connection_mut(token, |connection| {
                connection.destroy_connection_resources(outstanding_operations)
            })
            .ok_or(Error::TransportClosed)?
    }

    pub(super) fn destroy_failed_connection_install(
        &self,
        connections: &mut ConnectionRegistry,
        resources: &mut self::connection::FailedConnectionInstallResources,
    ) -> Result<(Option<SharedCmId>, bool)> {
        resources.destroy_for_session(connections)
    }

    pub(super) fn reject_failed_connection_install(
        &self,
        connections: &ConnectionRegistry,
        resources: &self::connection::FailedConnectionInstallResources,
    ) -> Result<()> {
        resources.reject_for_session(connections)
    }

    pub(super) fn track_connection_quarantine(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> bool {
        connections.track_bundle_quarantine(token)
    }

    pub(super) fn track_operation_quarantine(
        &self,
        connections: &mut ConnectionRegistry,
        connection: ConnectionToken,
        operation: OperationToken,
    ) -> bool {
        connections.track_operation_quarantine(connection, operation)
    }

    pub(super) fn clear_connection_quarantine(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> bool {
        if !connections.clear_bundle_quarantine(token) {
            return false;
        }
        true
    }

    pub(super) fn recover_connection_quarantine_entry(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
    ) -> bool {
        self.clear_connection_quarantine(connections, token)
    }

    pub(super) fn clear_operation_quarantine(
        &self,
        connections: &mut ConnectionRegistry,
        connection: ConnectionToken,
        operation: OperationToken,
    ) -> bool {
        if !connections.clear_operation_quarantine(connection, operation) {
            return false;
        }
        true
    }

    fn apply_io_effects(
        &self,
        connections: &mut ConnectionRegistry,
        mut effects: IoCoreEffects,
    ) -> CommittedIoCoreEffects {
        for effect in effects.take_quarantine() {
            match effect {
                OperationQuarantineEffect::Added {
                    connection,
                    operation,
                } => {
                    self.track_operation_quarantine(connections, connection, operation);
                }
                OperationQuarantineEffect::Cleared {
                    connection,
                    operation,
                } => {
                    self.clear_operation_quarantine(connections, connection, operation);
                }
            }
        }
        for token in effects.take_drained() {
            if connections.close_started(token) {
                self.recover_connection_quarantine(connections, token);
                self.record_connection_drained(connections, token);
                self.schedule_connection_retirement(connections, token);
            }
        }
        effects.into_committed()
    }

    /// Consume an I/O effect bundle, apply all session-facing mutations, and
    /// only then publish its detached events and operation wakes.
    ///
    /// Moving the bundle into this method prevents callers from publishing or
    /// reusing the original value. I/O producers return only after their
    /// operation and registry guards are released; direct posting and close
    /// paths use a separate detached-only type after their guards are dropped.
    /// This boundary cannot prove that a caller holds no unrelated lock.
    #[cfg(test)]
    pub(super) fn commit_io_effects(
        &self,
        connections: &mut ConnectionRegistry,
        effects: IoCoreEffects,
    ) {
        self.apply_io_effects(connections, effects).publish();
    }

    pub(super) fn commit_io_effects_into(
        &self,
        connections: &mut ConnectionRegistry,
        effects: IoCoreEffects,
        actions: &mut super::reactor::ReactorActions,
    ) {
        self.apply_io_effects(connections, effects)
            .append_to(actions);
    }

    /// Apply session effects for root terminal composition.
    ///
    /// The returned value contains only detached publication and must be
    /// consumed after CM and connection terminal state has been published.
    /// Reactor ownership confines conversion before root composition.
    pub(super) fn apply_terminal_io_effects(
        &self,
        connections: &mut ConnectionRegistry,
        effects: IoCoreEffects,
    ) -> CommittedIoCoreEffects {
        self.apply_io_effects(connections, effects)
    }

    pub(super) fn enqueue_completion_with_core(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut IoState,
        completion: crate::wc::WorkCompletion,
    ) -> Option<ConnectionToken> {
        let _admission = super::registry::read_unpoison(&self.frontend.admission);
        let pending = io_core.prepare_completion(completion)?;
        let identity = pending.identity();
        if !matches!(connections.lookup(identity.connection), Lookup::Occupied(_)) {
            io_core.reject_cqe(super::io_core::CqeReject::StaleConnection);
            return None;
        }
        let live = connections.prove_live_io(identity.connection, identity.qp_num);
        connections
            .with_connection_io_mut(identity.connection, |connection, connection_io, _poster| {
                io_core.enqueue_prepared_completion(pending, live, connection, connection_io)
            })
            .flatten()
    }

    pub(super) fn dispatch_connection_completions_with_core(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut IoState,
        token: ConnectionToken,
        quantum: usize,
        actions: &mut super::reactor::ReactorActions,
    ) -> (usize, bool) {
        let Some((processed, remains_ready, effects)) =
            connections.with_connection_io_mut(token, |connection, connection_io, _poster| {
                io_core.dispatch_connection_completions(connection, connection_io, quantum)
            })
        else {
            return (0, false);
        };
        self.commit_io_effects_into(connections, effects, actions);
        (processed, remains_ready)
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

    pub(super) fn reclaim_after_qp_destroy_into(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut IoState,
        proof: &QpDestructionProof,
        connection: ConnectionToken,
        tokens: Vec<OperationToken>,
        actions: &mut super::reactor::ReactorActions,
    ) -> usize {
        let proven_connection = proof.connection;
        let proven_qp_num = proof.qp_num;
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
                let Some((reclaimed, effects)) =
                    connections.with_connection_io_mut(connection, |io, connection_io, _poster| {
                        io_core.reclaim_after_qp_destroy(
                            proven_connection,
                            proven_qp_num,
                            io,
                            connection_io,
                            close_error.clone(),
                            *token,
                        )
                    })
                else {
                    return false;
                };
                self.commit_io_effects_into(connections, effects, actions);
                reclaimed
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
        self.commit_io_effects(connections, effects);
        reclaimed
    }

    pub(super) fn reject_queued_completions_after_qp_destroy_into(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut IoState,
        connection: ConnectionToken,
        actions: &mut super::reactor::ReactorActions,
    ) -> bool {
        // Every completion can publish an event, an operation wake, and a
        // connection-close wake. Keep two leaves for the owning connection's
        // quarantine/close tail before removing any copied CQE.
        let quantum = actions.remaining().saturating_sub(2) / 3;
        let Some((remains_ready, effects)) =
            connections.with_connection_io_mut(connection, |io, connection_io, _poster| {
                io_core.reject_queued_completions_after_qp_destroy(io, connection_io, quantum)
            })
        else {
            return false;
        };
        self.commit_io_effects_into(connections, effects, actions);
        remains_ready
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
            .manager
            .establish_qp_destruction_proof(&mut driver.reactor.session.connections, token)
            .expect("first successful destroy mints proof");
        assert_eq!(proof.connection, token);
        assert_eq!(proof.qp_num, connection.identity().qp_num());
        assert!(matches!(
            driver
                .reactor
                .session
                .manager
                .establish_qp_destruction_proof(
                    &mut driver.reactor.session.connections,
                    token,
                ),
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
