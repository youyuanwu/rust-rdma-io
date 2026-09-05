//! Connection/session ownership for the explicitly driven v2 engine.
//!
//! The session manager owns every CM route, listener, connection registry
//! entry, connection admission reservation, lifecycle deadline, and
//! connection-level quarantine entry. The sibling [`super::io_core::IoCore`]
//! owns operation/CQE state; effects crossing that boundary are interpreted
//! here before their detached events and wakers are published.

use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::ops::Deref;
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::Duration;

use self::cm::CmState;
pub(super) mod cm;
pub(super) mod connection;
mod drain;
pub(super) mod listener;
mod registry;

use self::connection::{
    ConnectionAdmissionPool, ConnectionState, QpDestroyStatus, SharedCmId, VerbsConnectionResources,
};
use self::listener::ListenerState;
use self::registry::ConnectionRegistry;
use super::driver::WorkSignal;
use super::io_core::{
    IoCore, IoCoreEffects, IoSessionBridge, OperationQuarantineEffect, QpReclaimCapability,
};
use super::registry::{ConnectionToken, Lookup, OperationToken, lock_unpoison};
use super::scheduler::{DeadlineKind, DeadlineRequest};
use super::{EngineShared, Result};
use crate::v2::error::Error;

/// Non-forgeable authority for connection and QP lifecycle transitions.
pub(super) struct SessionLifecycleAuthority {
    _private: (),
}

#[cfg(test)]
impl SessionLifecycleAuthority {
    pub(super) fn for_test() -> Self {
        Self { _private: () }
    }
}

/// Exact, non-cloneable proof minted after one successful synchronous QP destroy.
pub(super) struct QpDestructionProof {
    connection: ConnectionToken,
    qp_num: u32,
    _authority: (),
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

    pub(super) fn notify_waiters(&self) {
        self.notify.notify_waiters();
    }
}

/// Opaque request-only capability held by protocol I/O.
///
/// It cannot access a QP, CmId, registry entry, or lifecycle authority.
#[derive(Clone)]
pub(crate) struct SessionConnection {
    manager: Weak<SessionManager>,
    token: ConnectionToken,
    close: Arc<SessionCloseState>,
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

    pub(super) fn store_if_empty(&self, outcome: super::lifecycle::MemoizedTerminalResult) {
        let mut current = lock_unpoison(&self.outcome);
        if current.is_none() {
            *current = Some(outcome);
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

    pub(super) fn notify_waiters(&self) {
        self.notify.notify_waiters();
    }
}

/// Opaque request/observation capability for a SessionManager-owned listener.
#[derive(Clone)]
pub(super) struct SessionListener {
    manager: Weak<SessionManager>,
    listener: Weak<ListenerState>,
    close: Arc<SessionListenerCloseState>,
    local_addr: std::net::SocketAddr,
}

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

    pub(super) fn owners(&self) -> Result<(Arc<EngineShared>, Arc<ListenerState>)> {
        let manager = self.manager.upgrade().ok_or(Error::DriverShutdown)?;
        let engine = manager.engine().ok_or(Error::DriverShutdown)?;
        let listener = self.listener.upgrade().ok_or_else(|| {
            self.close
                .outcome()
                .and_then(|outcome| outcome.into_result().err())
                .unwrap_or(Error::TransportClosed)
        })?;
        Ok((engine, listener))
    }

    pub(super) fn request_close(&self) {
        let Ok((engine, listener)) = self.owners() else {
            return;
        };
        listener.request_close(&engine);
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
            if let Some(manager) = self.manager.upgrade()
                && let Some(outcome) = manager.engine_outcome()
            {
                return outcome.into_result();
            }
            notified.await;
        }
    }
}

impl SessionConnection {
    pub(super) fn new(
        manager: &Arc<SessionManager>,
        token: ConnectionToken,
        close: Arc<SessionCloseState>,
    ) -> Self {
        Self {
            manager: Arc::downgrade(manager),
            token,
            close,
        }
    }

    pub(crate) fn request_close(&self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.request_connection_close(self.token);
        }
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.request_close();
        loop {
            let notify = self.close.notify();
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.close.outcome() {
                return outcome.into_result();
            }

            if let Some(manager) = self.manager.upgrade()
                && let Some(outcome) = manager.engine_outcome()
            {
                return outcome.into_result();
            }
            notified.await;
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn transition_to_error_for_test(&self) -> Result<()> {
        let manager = self.manager.upgrade().ok_or(Error::DriverShutdown)?;
        manager.transition_connection_to_error_token(self.token)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum QuarantineKey {
    Connection(ConnectionToken),
    Operation(OperationToken),
}

#[derive(Clone, Copy)]
struct QuarantineEntry {
    connection: ConnectionToken,
}

#[derive(Default)]
struct QuarantineState {
    entries: HashMap<QuarantineKey, QuarantineEntry>,
    connection_entries: HashMap<ConnectionToken, usize>,
}

/// Sole owner of v2 connection/session state and lifecycle policy.
///
/// `IoCore` is retained only as the narrow operation side of the composition;
/// it has no dependency back on this owner. Resource-owning registry and CM
/// fields precede that retain so they are dropped before the I/O core can lose
/// its final session-held reference.
pub(super) struct SessionManager {
    pub(super) connection_admission: Arc<ConnectionAdmissionPool>,
    pub(super) connections: ConnectionRegistry,
    pub(super) cm: CmState,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cm_events: AtomicU64,
    deadline_requests: Mutex<VecDeque<DeadlineRequest>>,
    pub(super) admission: Arc<RwLock<()>>,
    pub(super) shutdown_connection_close_started: AtomicBool,
    quarantines: Mutex<QuarantineState>,
    engine: OnceLock<Weak<EngineShared>>,
    lifecycle_authority: SessionLifecycleAuthority,
    qp_reclaim: QpReclaimCapability,
    #[allow(
        dead_code,
        reason = "retained for session-owned close/reclaim service and test accounting adapters"
    )]
    pub(super) io_core: Arc<IoCore>,
}

impl SessionManager {
    pub(super) fn new(
        max_live_connections: usize,
        admission: Arc<RwLock<()>>,
        io_core: Arc<IoCore>,
        qp_reclaim: QpReclaimCapability,
    ) -> Result<Self> {
        Ok(Self {
            connection_admission: ConnectionAdmissionPool::new(max_live_connections),
            connections: ConnectionRegistry::new(max_live_connections)?,
            cm: CmState::new(max_live_connections)?,
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cm_events: AtomicU64::new(0),
            deadline_requests: Mutex::new(VecDeque::new()),
            admission,
            shutdown_connection_close_started: AtomicBool::new(false),
            quarantines: Mutex::new(QuarantineState::default()),
            engine: OnceLock::new(),
            lifecycle_authority: SessionLifecycleAuthority { _private: () },
            qp_reclaim,
            io_core,
        })
    }

    pub(super) fn bind_engine(&self, engine: &Arc<EngineShared>) {
        self.engine
            .set(Arc::downgrade(engine))
            .unwrap_or_else(|_| panic!("SessionManager is bound to exactly one EngineShared"));
    }

    pub(super) fn live_connection_count(&self) -> usize {
        self.connections.live()
    }

    pub(super) fn engine(&self) -> Option<Arc<EngineShared>> {
        self.engine.get().and_then(Weak::upgrade)
    }

    fn engine_outcome(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        self.engine().and_then(|engine| engine.outcome())
    }

    fn request_connection_close(&self, token: ConnectionToken) {
        let Some(engine) = self.engine() else {
            return;
        };
        let Lookup::Occupied(connection) = self.connections.lookup(token) else {
            return;
        };
        self.begin_connection_close(&engine, &connection);
    }

    pub(super) fn connection_capability(
        self: &Arc<Self>,
        connection: &ConnectionState,
    ) -> SessionConnection {
        SessionConnection::new(self, connection.token, connection.close_state())
    }

    pub(super) fn listener_capability(
        self: &Arc<Self>,
        listener: &Arc<ListenerState>,
    ) -> SessionListener {
        SessionListener {
            manager: Arc::downgrade(self),
            listener: Arc::downgrade(listener),
            close: listener.close_state(),
            local_addr: listener.local_addr,
        }
    }

    pub(super) fn establish_qp_destruction_proof(
        &self,
        connection: &ConnectionState,
        lifecycle: &std::sync::MutexGuard<'_, ()>,
    ) -> Result<QpDestructionProof> {
        match connection.destroy_qp_for_session(&self.lifecycle_authority, lifecycle)? {
            QpDestroyStatus::DestroyedNow => Ok(QpDestructionProof {
                connection: connection.token,
                qp_num: connection.qp_num(),
                _authority: (),
            }),
            QpDestroyStatus::AlreadyDestroyed => Err(Error::InvalidConfig(
                "QP destruction proof was already minted and cannot be replayed".into(),
            )),
        }
    }

    pub(super) fn ensure_qp_destroyed(
        &self,
        connection: &ConnectionState,
        lifecycle: &std::sync::MutexGuard<'_, ()>,
    ) -> Result<()> {
        match connection.destroy_qp_for_session(&self.lifecycle_authority, lifecycle)? {
            QpDestroyStatus::DestroyedNow | QpDestroyStatus::AlreadyDestroyed => Ok(()),
        }
    }

    pub(super) fn transition_connection_to_error(
        &self,
        connection: &ConnectionState,
    ) -> Result<bool> {
        connection.transition_to_error_once(&self.lifecycle_authority)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn transition_connection_to_error_token(&self, token: ConnectionToken) -> Result<()> {
        let Lookup::Occupied(connection) = self.connections.lookup(token) else {
            return Err(Error::TransportClosed);
        };
        self.transition_connection_to_error(&connection).map(|_| ())
    }

    pub(super) fn finalize_connection_engine(
        &self,
        connection: &ConnectionState,
        outcome: &super::lifecycle::MemoizedTerminalResult,
    ) -> Option<super::io::PendingIoEvent> {
        connection.close_state().record_engine_terminal(outcome);
        connection.finalize_engine(&self.lifecycle_authority, outcome)
    }

    pub(super) fn destroy_connection_resources(
        &self,
        connection: &ConnectionState,
        lifecycle: &std::sync::MutexGuard<'_, ()>,
    ) -> Result<Option<SharedCmId>> {
        connection.destroy_connection_resources(&self.lifecycle_authority, lifecycle)
    }

    pub(super) fn destroy_unregistered_connection(
        &self,
        connection: &VerbsConnectionResources,
    ) -> Result<(Option<SharedCmId>, bool)> {
        connection.destroy_unregistered_for_session(&self.lifecycle_authority)
    }

    #[cfg(test)]
    pub(super) fn mint_qp_destruction_proof_for_test(
        &self,
        connection: &ConnectionState,
    ) -> QpDestructionProof {
        connection.record_qp_destroyed_for_test();
        QpDestructionProof {
            connection: connection.token,
            qp_num: connection.qp_num(),
            _authority: (),
        }
    }

    pub(super) fn schedule_deadline(
        &self,
        work_signal: &WorkSignal,
        kind: DeadlineKind,
        token: u64,
        after: Duration,
    ) {
        let now = tokio::time::Instant::now();
        let at = now.checked_add(after).unwrap_or(now);
        lock_unpoison(&self.deadline_requests).push_back(DeadlineRequest { at, kind, token });
        work_signal.publish(super::driver::RECLAMATION_WORK);
    }

    pub(super) fn take_deadline_requests(&self, budget: usize) -> Vec<DeadlineRequest> {
        let mut requests = lock_unpoison(&self.deadline_requests);
        let count = requests.len().min(budget);
        requests.drain(..count).collect()
    }

    pub(super) fn has_deadline_requests(&self) -> bool {
        !lock_unpoison(&self.deadline_requests).is_empty()
    }

    pub(super) fn track_connection_quarantine(&self, token: ConnectionToken) -> bool {
        self.track_quarantine(
            QuarantineKey::Connection(token),
            QuarantineEntry { connection: token },
        )
    }

    pub(super) fn track_operation_quarantine(
        &self,
        operation: OperationToken,
        connection: ConnectionToken,
    ) -> bool {
        self.track_quarantine(
            QuarantineKey::Operation(operation),
            QuarantineEntry { connection },
        )
    }

    fn track_quarantine(&self, key: QuarantineKey, entry: QuarantineEntry) -> bool {
        let mut quarantines = lock_unpoison(&self.quarantines);
        if quarantines.entries.contains_key(&key) {
            return false;
        }
        quarantines.entries.insert(key, entry);
        let connection_entries = quarantines
            .connection_entries
            .entry(entry.connection)
            .or_insert(0);
        let first_for_connection = *connection_entries == 0;
        *connection_entries += 1;
        if first_for_connection
            && let Lookup::Occupied(connection) = self.connections.lookup(entry.connection)
        {
            connection.mark_reservation_quarantined();
        }
        first_for_connection
    }

    pub(super) fn clear_connection_quarantine(&self, token: ConnectionToken) -> bool {
        self.clear_quarantine(QuarantineKey::Connection(token), token)
    }

    pub(super) fn recover_connection_quarantine_entry(&self, token: ConnectionToken) -> bool {
        self.clear_quarantine(QuarantineKey::Connection(token), token)
    }

    pub(super) fn clear_operation_quarantine(
        &self,
        operation: OperationToken,
        connection: ConnectionToken,
    ) -> bool {
        self.clear_quarantine(QuarantineKey::Operation(operation), connection)
    }

    fn clear_quarantine(&self, key: QuarantineKey, connection: ConnectionToken) -> bool {
        let mut quarantines = lock_unpoison(&self.quarantines);
        if quarantines.entries.remove(&key).is_none() {
            return false;
        }
        let Some(connection_entries) = quarantines.connection_entries.get_mut(&connection) else {
            debug_assert!(false, "quarantine entry must have a connection count");
            return false;
        };
        *connection_entries -= 1;
        if *connection_entries != 0 {
            return false;
        }
        if let Lookup::Occupied(connection) = self.connections.lookup(connection) {
            connection.recover_reservation_quarantine();
        } else {
            self.connection_admission.clear_retained_quarantine();
        }
        quarantines.connection_entries.remove(&connection);
        true
    }

    /// Consume session-facing I/O effects before detached publication.
    pub(super) fn apply_io_effects(&self, shared: &EngineShared, effects: &mut IoCoreEffects) {
        for effect in effects.take_quarantine() {
            match effect {
                OperationQuarantineEffect::Added {
                    operation,
                    connection,
                } => {
                    self.track_operation_quarantine(operation, connection);
                }
                OperationQuarantineEffect::Cleared {
                    operation,
                    connection,
                } => {
                    self.clear_operation_quarantine(operation, connection);
                }
            }
        }
        for token in effects.take_drained() {
            let Lookup::Occupied(connection) = self.connections.lookup(token) else {
                continue;
            };
            if connection.close_started() && connection.accepted_count() == 0 {
                self.recover_connection_quarantine(&connection);
                self.record_connection_drained(&connection);
                self.schedule_connection_retirement(shared, &connection);
            }
        }
    }

    pub(super) fn enqueue_completion(
        &self,
        completion: crate::wc::WorkCompletion,
    ) -> Option<ConnectionToken> {
        let _admission = super::registry::read_unpoison(&self.admission);
        let pending = self.io_core.prepare_completion(completion)?;
        let identity = pending.identity();
        let connection = match self.connections.lookup(identity.connection) {
            Lookup::Occupied(connection) => connection,
            _ => {
                self.io_core
                    .reject_cqe(super::io_core::CqeReject::StaleConnection);
                return None;
            }
        };
        let live = self
            .connections
            .prove_live_io(identity.connection, identity.qp_num);
        self.io_core
            .enqueue_prepared_completion(pending, live, &connection.io)
    }

    pub(super) fn dispatch_connection_completions(
        &self,
        shared: &EngineShared,
        token: ConnectionToken,
        quantum: usize,
    ) -> (usize, bool) {
        let connection = match self.connections.lookup(token) {
            Lookup::Occupied(connection) => connection,
            _ => return (0, false),
        };
        let (processed, remains_ready, mut effects) = self
            .io_core
            .dispatch_connection_completions(&connection.io, quantum);
        self.apply_io_effects(shared, &mut effects);
        effects.publish();
        (processed, remains_ready)
    }

    pub(super) fn reclaim_after_qp_destroy(
        &self,
        shared: &EngineShared,
        proof: QpDestructionProof,
        connection: &ConnectionState,
        tokens: Vec<OperationToken>,
    ) -> usize {
        let QpDestructionProof {
            connection: proven_connection,
            qp_num: proven_qp_num,
            _authority: (),
        } = proof;
        if proven_connection != connection.token || proven_qp_num != connection.qp_num() {
            tracing::warn!(
                connection = connection.token.encode(),
                "operation reclaim rejected a mismatched QP destruction proof"
            );
            return 0;
        }
        tokens
            .into_iter()
            .filter(|token| {
                self.reclaim_after_proven_qp_destroy(
                    shared,
                    proven_connection,
                    proven_qp_num,
                    connection,
                    *token,
                )
            })
            .count()
    }

    fn reclaim_after_proven_qp_destroy(
        &self,
        shared: &EngineShared,
        proven_connection: ConnectionToken,
        proven_qp_num: u32,
        connection: &ConnectionState,
        token: OperationToken,
    ) -> bool {
        let (reclaimed, mut effects) = self.qp_reclaim.reclaim(
            proven_connection,
            proven_qp_num,
            &connection.io,
            connection.operation_close_error(),
            token,
        );
        self.apply_io_effects(shared, &mut effects);
        effects.publish();
        reclaimed
    }

    #[cfg(test)]
    pub(super) fn reclaim_after_qp_destroy_for_test(
        &self,
        shared: &EngineShared,
        proof: &QpDestructionProof,
        connection: &ConnectionState,
        token: OperationToken,
    ) -> bool {
        self.reclaim_after_proven_qp_destroy(
            shared,
            proof.connection,
            proof.qp_num,
            connection,
            token,
        )
    }

    pub(super) fn reject_queued_completions_after_qp_destroy(
        &self,
        shared: &EngineShared,
        connection: &ConnectionState,
    ) -> bool {
        let (remains_ready, mut effects) = self
            .io_core
            .reject_queued_completions_after_qp_destroy(&connection.io);
        self.apply_io_effects(shared, &mut effects);
        effects.publish();
        remains_ready
    }

    pub(super) fn handle_reclamation_deadline(&self, shared: &EngineShared, token: OperationToken) {
        let mut effects = self.io_core.handle_reclamation_deadline(token);
        self.apply_io_effects(shared, &mut effects);
        effects.publish();
    }

    pub(super) fn quarantine_operation(&self, shared: &EngineShared, token: OperationToken) {
        let mut effects = self.io_core.quarantine_operation(token);
        self.apply_io_effects(shared, &mut effects);
        effects.publish();
    }
}

impl IoSessionBridge for SessionManager {
    fn route_completion(&self, completion: crate::wc::WorkCompletion) -> Option<ConnectionToken> {
        self.enqueue_completion(completion)
    }

    fn dispatch_connection_completions(
        &self,
        connection: ConnectionToken,
        quantum: usize,
    ) -> (usize, bool) {
        let Some(shared) = self.engine() else {
            return (0, false);
        };
        self.dispatch_connection_completions(&shared, connection, quantum)
    }

    fn handle_reclamation_deadline(&self, token: OperationToken) {
        let Some(shared) = self.engine() else {
            return;
        };
        self.handle_reclamation_deadline(&shared, token);
    }
}

#[cfg(test)]
impl Deref for SessionManager {
    // Existing colocated unit tests exercise exact I/O accounting through
    // their synthetic engine. This adapter is absent from production builds.
    type Target = IoCore;

    fn deref(&self) -> &Self::Target {
        &self.io_core
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};
    use std::time::Duration;

    use super::super::registry::{ConnectionToken, lock_unpoison};
    use super::super::scheduler::DeadlineKind;
    use super::super::{CompletionMode, RdmaConnectionConfig, test_engine_pair};
    use super::connection::{WorkRequestPoster, install_connection};
    use super::listener::{ListenerState, RdmaListener};
    use super::{SessionCloseState, SessionConnection};
    use crate::v2::error::{Error, Result};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct TestPoster {
        qp_num: u32,
    }

    impl WorkRequestPoster for TestPoster {
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
    fn session_manager_owns_registry_admission_cm_and_deadlines() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let manager = &engine.shared.session;

        assert_eq!(manager.connections.live(), 0);
        assert_eq!(manager.connection_admission.snapshot().live, 0);
        assert_eq!(manager.cm.pending_route_count(), 0);
        assert!(!manager.has_deadline_requests());

        manager.schedule_deadline(
            &engine.shared.work_signal,
            DeadlineKind::ConnectionDrain,
            7,
            Duration::ZERO,
        );
        assert!(manager.has_deadline_requests());
        let requests = manager.take_deadline_requests(1);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].kind, DeadlineKind::ConnectionDrain);
        assert_eq!(requests[0].token, 7);
        assert!(!manager.has_deadline_requests());
    }

    #[test]
    fn session_connection_capability_is_resource_free_and_routes_close() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let connection = install_connection(
            &engine.shared,
            Arc::new(TestPoster { qp_num: 17 }),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .expect("install synthetic connection");
        let state_retain_count = Arc::strong_count(&connection.state);
        let capability = engine
            .shared
            .session
            .connection_capability(&connection.state);

        assert_eq!(
            Arc::strong_count(&connection.state),
            state_retain_count,
            "request capability must not retain ConnectionState or its resource bundle"
        );
        capability.request_close();
        assert!(connection.state.close_started());
        assert!(connection.state.error_transition_complete());
    }

    #[test]
    fn session_connection_close_observer_waits_for_retirement_after_cm_failure() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let connection = install_connection(
            &engine.shared,
            Arc::new(TestPoster { qp_num: 18 }),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .expect("install synthetic connection");
        let capability = engine
            .shared
            .session
            .connection_capability(&connection.state);

        let _pending = connection.state.mark_cm_failure(Error::TransportClosed);
        assert!(
            capability.close.outcome().is_none(),
            "ordinary close errors remain hidden until QP/CmId retirement"
        );
        let _pending = connection.state.finish_retirement();
        assert!(matches!(
            capability.close.outcome().unwrap().into_result(),
            Err(Error::TransportClosed)
        ));
    }

    #[tokio::test]
    async fn session_connection_close_observes_engine_terminal_without_manager_owner() {
        let close = SessionCloseState::new();
        *lock_unpoison(&close.outcome) = Some(
            super::super::lifecycle::MemoizedTerminalResult::from_error(Error::TransportClosed),
        );
        close.record_engine_terminal(
            &super::super::lifecycle::MemoizedTerminalResult::from_error(Error::DriverShutdown),
        );
        let capability = SessionConnection {
            manager: Weak::new(),
            token: ConnectionToken {
                slot: 0,
                generation: 1,
            },
            close,
        };

        assert!(matches!(
            capability.close().await,
            Err(Error::DriverShutdown)
        ));
    }

    #[test]
    fn session_listener_capability_is_resource_free() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let state = ListenerState::test_only(4);
        let before = Arc::strong_count(&state);
        let listener = RdmaListener::from_state(&engine.shared, Arc::clone(&state));

        assert_eq!(listener.local_addr().unwrap(), state.local_addr);
        assert_eq!(
            Arc::strong_count(&state),
            before,
            "listener capability must not retain ListenerState or its CmId"
        );
        let clone = listener.clone();
        assert_eq!(Arc::strong_count(&state), before);
        drop(clone);
        drop(listener);
        assert_eq!(
            Arc::strong_count(&state),
            before + 1,
            "last-frontend close transfers the only added retain to SessionManager CM work"
        );
    }

    #[test]
    fn session_lifecycle_authority_mints_one_exact_qp_proof() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let connection = install_connection(
            &engine.shared,
            Arc::new(TestPoster { qp_num: 19 }),
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .expect("install synthetic connection");
        let lifecycle = connection.state.lock_lifecycle();
        let proof = engine
            .shared
            .session
            .establish_qp_destruction_proof(&connection.state, &lifecycle)
            .expect("first successful destroy mints proof");
        assert_eq!(proof.connection, connection.state.token);
        assert_eq!(proof.qp_num, connection.state.qp_num());
        assert!(matches!(
            engine
                .shared
                .session
                .establish_qp_destruction_proof(&connection.state, &lifecycle),
            Err(Error::InvalidConfig(message)) if message.contains("cannot be replayed")
        ));
        drop(lifecycle);

        assert_eq!(
            engine.shared.session.reclaim_after_qp_destroy(
                &engine.shared,
                proof,
                &connection.state,
                Vec::new(),
            ),
            0
        );
    }
}
