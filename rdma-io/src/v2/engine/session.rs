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
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::Duration;

use super::cm::CmState;
use super::connection::{ConnectionAdmissionPool, ConnectionState};
use super::driver::WorkSignal;
use super::io_core::{IoCore, IoCoreEffects, OperationQuarantineEffect};
use super::registry::{ConnectionRegistry, ConnectionToken, Lookup, OperationToken, lock_unpoison};
use super::scheduler::{DeadlineKind, DeadlineRequest};
use super::{EngineShared, Result};

/// Resource-free close observation shared with connection frontends.
pub(super) struct SessionCloseState {
    pub(super) outcome: Mutex<Option<super::lifecycle::MemoizedTerminalResult>>,
    pub(super) notify: Arc<tokio::sync::Notify>,
}

impl SessionCloseState {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(None),
            notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    pub(super) fn notify(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    pub(super) fn outcome(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        lock_unpoison(&self.outcome).clone()
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
    #[allow(
        dead_code,
        reason = "retained for session-owned close/reclaim service and test accounting adapters"
    )]
    io_core: Arc<IoCore>,
}

impl SessionManager {
    pub(super) fn new(
        max_live_connections: usize,
        admission: Arc<RwLock<()>>,
        io_core: Arc<IoCore>,
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
            io_core,
        })
    }

    pub(super) fn bind_engine(&self, engine: &Arc<EngineShared>) {
        self.engine
            .set(Arc::downgrade(engine))
            .unwrap_or_else(|_| panic!("SessionManager is bound to exactly one EngineShared"));
    }

    fn engine(&self) -> Option<Arc<EngineShared>> {
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
        engine.begin_connection_close(&connection);
    }

    pub(super) fn connection_capability(
        self: &Arc<Self>,
        connection: &ConnectionState,
    ) -> SessionConnection {
        SessionConnection::new(self, connection.token, connection.close_state())
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
                shared.recover_connection_quarantine(&connection);
                shared.record_connection_drained(&connection);
                shared.schedule_connection_retirement(&connection);
            }
        }
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
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::connection::{WorkRequestPoster, install_connection};
    use super::super::scheduler::DeadlineKind;
    use super::super::{CompletionMode, RdmaConnectionConfig, test_engine_pair};
    use crate::v2::error::Result;
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
}
