//! Engine-owned low-level connection frontend.

use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::RwLockReadGuard;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::OwnedSemaphorePermit;

use self::qp::QpCapabilitiesExt;
use super::super::RdmaConnectionConfig;
use super::super::io::{IoEventSender, IoTerminalEvent, MemoryRegistrar, PendingIoEvent};
use super::super::io_core::RdmaOperation;
use super::super::io_core::{
    ConnectionIoState, EstablishedIoConnection, EstablishedIoIdentity, IoQuarantineReport,
    OperationKind,
};
use super::super::lifecycle::MemoizedTerminalResult;
#[cfg(any(test, feature = "test-hooks"))]
use super::super::registry::OperationToken;
use super::super::registry::{ConnectionToken, lock_unpoison, read_unpoison};
use super::registry::ConnectionRegistry;
use super::{QpDestructionProof, SessionCloseState, SessionFrontend, SessionManager};
use crate::cm::{CmId, ConnParam, EventChannel};
use crate::v2::error::{Error, Result};
use crate::v2::mr::{AccessIntent, Mr, RemoteMr};
use crate::v2::qp::{BatchPostOutcome, Qp, QpCapabilities};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

/// Non-owning connection identity suitable for diagnostics and correlation.
///
/// # Use case
///
/// Compare, hash, and log connection identity while observing its `qp_num`.
///
/// # Ownership and progress
///
/// The copied value retains no connection or engine ownership.
///
/// # Safety and limits
///
/// Registry slot and generation remain private so callers cannot construct
/// stale routing identities.
///
/// # Availability
///
/// Returned by [`RdmaConnection::identity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RdmaConnectionIdentity {
    slot: u32,
    generation: u32,
    qp_num: u32,
}

impl RdmaConnectionIdentity {
    /// Return the provider-reported queue-pair number.
    pub fn qp_num(&self) -> u32 {
        self.qp_num
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn registry_slot(&self) -> u32 {
        self.slot
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn registration_generation(&self) -> u32 {
        self.generation
    }
}

/// Engine-owned low-level RDMA connection.
///
/// The connection exposes owned operation futures but no raw PD, QP, CQ, CM,
/// or independently pollable completion-driver handle. Establishment posts
/// zero initial receives.
pub struct RdmaConnection {
    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) state: Arc<ConnectionTestAccess>,
    pub(in crate::v2::engine) memory: MemoryRegistrar,
    commands: Weak<super::super::reactor::CommandIngress>,
    session_frontend: Weak<SessionFrontend>,
    close: Arc<SessionCloseState>,
    frontend: Arc<ConnectionFrontendState>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    identity: RdmaConnectionIdentity,
    route: Option<ConnectionCmRoute>,
}

impl Clone for RdmaConnection {
    fn clone(&self) -> Self {
        self.frontend.count.fetch_add(1, Ordering::Relaxed);
        Self {
            #[cfg(any(test, feature = "test-hooks"))]
            state: Arc::clone(&self.state),
            memory: self.memory.clone(),
            commands: self.commands.clone(),
            session_frontend: self.session_frontend.clone(),
            close: Arc::clone(&self.close),
            frontend: Arc::clone(&self.frontend),
            local_addr: self.local_addr,
            peer_addr: self.peer_addr,
            identity: self.identity,
            route: self.route,
        }
    }
}

impl RdmaConnection {
    /// Register owned memory against this engine's shared protection domain.
    ///
    /// The returned MR remains owned by the caller until it is submitted in an
    /// [`RdmaOperation`]. Length must be nonzero and fit the provider ABI.
    pub fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        self.memory.register(len, access)
    }

    /// Create a two-sided SEND operation admitted on first poll and posted by
    /// a later engine-driver poll.
    ///
    /// The optional `(offset, length)` selects a checked MR range. Awaiting the
    /// future returns `(Result<Completion>, Option<Mr>)`.
    pub fn send(&self, mr: Mr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            self.commands.clone(),
            self.session_frontend.clone(),
            self.session_token(),
            OperationKind::Send,
            mr,
            None,
            range,
        )
    }

    /// Create a two-sided RECV operation admitted on first poll and posted by
    /// a later engine-driver poll.
    pub fn recv(&self, mr: Mr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            self.commands.clone(),
            self.session_frontend.clone(),
            self.session_token(),
            OperationKind::Recv,
            mr,
            None,
            range,
        )
    }

    /// Create an RDMA WRITE operation admitted on first poll and posted by a
    /// later engine-driver poll.
    pub fn write(&self, mr: Mr, remote: RemoteMr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            self.commands.clone(),
            self.session_frontend.clone(),
            self.session_token(),
            OperationKind::Write,
            mr,
            Some(remote),
            range,
        )
    }

    /// Create an RDMA READ operation admitted on first poll and posted by a
    /// later engine-driver poll.
    pub fn read(&self, mr: Mr, remote: RemoteMr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            self.commands.clone(),
            self.session_frontend.clone(),
            self.session_token(),
            OperationKind::Read,
            mr,
            Some(remote),
            range,
        )
    }

    /// Return the local socket address reported by RDMA-CM.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.local_addr
            .ok_or_else(|| Error::InvalidConfig("connection local address is unavailable".into()))
    }

    /// Return the peer socket address reported by RDMA-CM.
    pub fn peer_addr(&self) -> Result<SocketAddr> {
        self.peer_addr
            .ok_or_else(|| Error::InvalidConfig("connection peer address is unavailable".into()))
    }

    /// Return the opaque current connection identity and exact `qp_num`.
    pub fn identity(&self) -> RdmaConnectionIdentity {
        self.identity
    }

    /// Stop new posting and wait for the exact accepted set to drain safely.
    ///
    /// A successful close retires the CM route and connection registry
    /// generation, destroys the QP before its CM ID, and returns aggregate
    /// admission once. The engine first consumes real exact CQEs. If the
    /// provider omits flush CQEs through the drain deadline, it synchronously
    /// destroys the owning per-connection QP while its CM ID remains alive;
    /// only that completed destruction permits unresolved operations and MRs
    /// to be reclaimed. Quarantine is reserved for inability to establish that
    /// destruction boundary or another retirement wedge. Peer disconnect uses
    /// the same local QP-to-ERR and safe-destruction path.
    pub async fn close(&self) -> Result<()> {
        self.request_close();
        loop {
            let notify = self.close.notify();
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.close.outcome() {
                return outcome.into_result();
            }
            if let Some(frontend) = self.session_frontend.upgrade()
                && let Some(outcome) = frontend.engine_outcome()
            {
                return outcome.into_result();
            }
            notified.await;
        }
    }
}

impl Drop for RdmaConnection {
    fn drop(&mut self) {
        let previous = self.frontend.count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "connection frontend count must be positive");
        if previous == 1 && !self.close.is_retired() {
            self.request_close();
        }
    }
}

impl RdmaConnection {
    pub(in crate::v2::engine) fn session_token(&self) -> ConnectionToken {
        ConnectionToken {
            slot: self.identity.slot,
            generation: self.identity.generation,
        }
    }

    pub(in crate::v2::engine) fn command_ingress(
        &self,
    ) -> Weak<super::super::reactor::CommandIngress> {
        self.commands.clone()
    }

    pub(in crate::v2::engine) fn frontend(&self) -> Weak<SessionFrontend> {
        self.session_frontend.clone()
    }

    pub(in crate::v2::engine) fn close_state(&self) -> Arc<SessionCloseState> {
        Arc::clone(&self.close)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn require_session_state(&self) -> Result<Arc<ConnectionTestAccess>> {
        Ok(Arc::clone(&self.state))
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn cm_route(&self) -> Option<ConnectionCmRoute> {
        self.route
    }

    pub(in crate::v2::engine) fn request_close(&self) {
        if self.close.is_retired() {
            return;
        }
        if let (Some(frontend), Some(commands)) =
            (self.session_frontend.upgrade(), self.commands.upgrade())
        {
            commands.request_connection_close(&frontend, self.session_token());
        }
    }

    fn from_registered(
        manager: &SessionManager,
        state: &ConnectionState,
        route: Option<ConnectionCmRoute>,
    ) -> Self {
        let frontend = manager.frontend();
        let identity = state.identity();
        let local_addr = state.local_addr;
        let peer_addr = state.peer_addr;
        Self {
            #[cfg(any(test, feature = "test-hooks"))]
            state: Arc::new(ConnectionTestAccess {
                token: state.token,
                io: Arc::clone(&state.io),
                #[cfg(test)]
                close: Arc::clone(&state.close),
                uses_engine_resources: state.poster.uses_engine_resources(),
            }),
            memory: frontend.memory_registrar(),
            commands: frontend
                .commands
                .get()
                .expect("SessionFrontend command ingress is bound before use")
                .clone(),
            session_frontend: frontend
                .self_ref
                .get()
                .expect("SessionFrontend self reference is bound before use")
                .clone(),
            close: state.close_state(),
            frontend: Arc::clone(&state.frontend),
            local_addr,
            peer_addr,
            identity,
            route,
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
pub(in crate::v2::engine) struct ConnectionTestAccess {
    pub(in crate::v2::engine) token: ConnectionToken,
    pub(in crate::v2::engine) io: Arc<EstablishedIoConnection>,
    #[cfg(test)]
    close: Arc<SessionCloseState>,
    uses_engine_resources: bool,
}

#[cfg(any(test, feature = "test-hooks"))]
impl ConnectionTestAccess {
    #[cfg(test)]
    pub(in crate::v2::engine) fn close_state(&self) -> Arc<SessionCloseState> {
        Arc::clone(&self.close)
    }

    pub(in crate::v2::engine) fn accepted_tokens(&self) -> Vec<OperationToken> {
        self.io.accepted_tokens_for_observation()
    }

    pub(in crate::v2::engine) fn uses_resources_for_test(
        &self,
        _pd: &crate::v2::Pd,
        _cq: &crate::v2::Cq,
    ) -> bool {
        self.uses_engine_resources
    }
}

pub(in crate::v2::engine) struct ConnectionFrontendState {
    count: AtomicUsize,
}

pub(in crate::v2::engine) struct ConnectionState {
    pub(in crate::v2::engine) token: ConnectionToken,
    qp_num: u32,
    poster: ConnectionPoster,
    pub(in crate::v2::engine) io: Arc<EstablishedIoConnection>,
    pub(in crate::v2::engine) io_ledger: ConnectionIoState,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    io_closed: bool,
    close_operation_scan_slot: usize,
    close_operation_scan_complete: bool,
    quarantine_operation_scan_slot: usize,
    quarantine_operation_scan_complete: bool,
    close: Arc<SessionCloseState>,
    close_result: Option<MemoizedTerminalResult>,
    inbound_accept_succeeded: bool,
    error_transition_started: bool,
    error_transition_complete: bool,
    qp_destroyed: bool,
    qp_reclamation_proof: Option<QpDestructionProof>,
    frontend: Arc<ConnectionFrontendState>,
    drained_recorded: bool,
    pub(super) admission: Option<ConnectionReservation>,
    #[cfg(any(test, feature = "test-hooks"))]
    retained_setup_rollback_mr: Option<Mr>,
}

pub(in crate::v2::engine) enum QpDestroyStatus {
    DestroyedNow,
    AlreadyDestroyed,
}

impl ConnectionState {
    pub(super) fn new(
        token: ConnectionToken,
        poster: impl Into<ConnectionPoster>,
        config: RdmaConnectionConfig,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
        mut admission: Option<ConnectionReservation>,
    ) -> Self {
        if let Some(reservation) = admission.as_mut() {
            reservation.mark_registered();
        }
        let poster = poster.into();
        let qp_num = poster.qp_num();
        let close = SessionCloseState::new();
        let close_notify = close.notify();
        let io = EstablishedIoConnection::new(
            EstablishedIoIdentity {
                connection: token,
                qp_num,
            },
            config.max_send_wr,
            config.max_recv_wr,
            Arc::clone(&close_notify),
        );
        let io_ledger = ConnectionIoState::from_connection(&io);
        Self {
            token,
            qp_num,
            poster,
            io,
            io_ledger,
            local_addr,
            peer_addr,
            io_closed: false,
            close_operation_scan_slot: 0,
            close_operation_scan_complete: false,
            quarantine_operation_scan_slot: 0,
            quarantine_operation_scan_complete: false,
            close,
            close_result: None,
            inbound_accept_succeeded: false,
            error_transition_started: false,
            error_transition_complete: false,
            qp_destroyed: false,
            qp_reclamation_proof: None,
            frontend: Arc::new(ConnectionFrontendState {
                count: AtomicUsize::new(1),
            }),
            drained_recorded: false,
            admission,
            #[cfg(any(test, feature = "test-hooks"))]
            retained_setup_rollback_mr: None,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn new_for_test(
        token: ConnectionToken,
        poster: impl Into<ConnectionPoster>,
        config: RdmaConnectionConfig,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
        admission: Option<ConnectionReservation>,
    ) -> Self {
        Self::new(token, poster, config, local_addr, peer_addr, admission)
    }

    pub(in crate::v2::engine) fn identity(&self) -> RdmaConnectionIdentity {
        let io = self.io.identity();
        debug_assert_eq!(io.connection, self.token);
        debug_assert_eq!(io.qp_num, self.qp_num);
        RdmaConnectionIdentity {
            slot: io.connection.slot,
            generation: io.connection.generation,
            qp_num: io.qp_num,
        }
    }

    pub(in crate::v2::engine) fn qp_num(&self) -> u32 {
        self.qp_num
    }

    pub(in crate::v2::engine) fn connect(&self, param: &ConnParam) -> Result<()> {
        self.poster.connect(param)
    }

    pub(in crate::v2::engine) fn accept_inbound(&mut self, param: &ConnParam) -> Result<()> {
        if self.inbound_accept_succeeded {
            return Err(Error::InvalidConfig(
                "inbound provider accept was requested more than once".into(),
            ));
        }
        self.poster.accept(param)?;
        self.inbound_accept_succeeded = true;
        Ok(())
    }

    pub(in crate::v2::engine) fn inbound_accept_succeeded(&self) -> bool {
        self.inbound_accept_succeeded
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn record_inbound_accept_for_test(&mut self) {
        assert!(
            !self.inbound_accept_succeeded,
            "test inbound accept evidence is recorded once"
        );
        self.inbound_accept_succeeded = true;
    }

    pub(in crate::v2::engine) fn reject(&self) -> Result<()> {
        self.poster.reject()
    }

    pub(in crate::v2::engine) fn io_parts_mut(
        &mut self,
    ) -> (
        &Arc<EstablishedIoConnection>,
        &mut ConnectionIoState,
        &ConnectionPoster,
    ) {
        (&self.io, &mut self.io_ledger, &self.poster)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn disconnect_for_test(&self) -> Result<()> {
        self.poster.disconnect_for_test()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn fail_next_qp_destroy_for_test(&self) -> Result<()> {
        self.poster.fail_next_qp_destroy()
    }

    pub(in crate::v2::engine) fn install_io_event_sender(
        &self,
        sender: IoEventSender,
    ) -> Result<Option<PendingIoEvent>> {
        if !self.io.install_io_event_sender(sender, self.io_closed)? {
            return Ok(None);
        }

        let error = self.operation_close_error();
        Ok(
            self.pending_io_event(if matches!(error, Error::TransportClosed) {
                IoTerminalEvent::Disconnected
            } else {
                IoTerminalEvent::Terminal(error)
            }),
        )
    }

    pub(in crate::v2::engine) fn close_state(&self) -> Arc<SessionCloseState> {
        Arc::clone(&self.close)
    }

    fn pending_io_event(&self, event: IoTerminalEvent) -> Option<PendingIoEvent> {
        self.io.pending_io_event(event)
    }

    pub(in crate::v2::engine) fn stop_posting(&mut self) {
        self.io_closed = true;
    }

    pub(in crate::v2::engine) fn io_is_open(&self) -> bool {
        !self.io_closed
    }

    pub(in crate::v2::engine) fn finalize_engine(
        &mut self,
        outcome: &MemoizedTerminalResult,
    ) -> Option<PendingIoEvent> {
        self.stop_posting();
        let _ = self.transition_to_error_once();
        if let Some(error) = outcome.error() {
            if self.close_result.is_none() {
                self.close_result = Some(MemoizedTerminalResult::from_error(error.clone()));
            }
            self.publish_close_result();
            return self.pending_io_event(IoTerminalEvent::Terminal(error));
        }
        None
    }

    pub(in crate::v2::engine) fn finalize_engine_without_provider(
        &mut self,
        outcome: &MemoizedTerminalResult,
    ) -> Option<PendingIoEvent> {
        self.stop_posting();
        if let Some(error) = outcome.error() {
            if self.close_result.is_none() {
                self.close_result = Some(MemoizedTerminalResult::from_error(error.clone()));
            }
            self.publish_close_result();
            return self.pending_io_event(IoTerminalEvent::Terminal(error));
        }
        None
    }

    pub(in crate::v2::engine) fn mark_disconnected(&mut self) -> Option<PendingIoEvent> {
        self.stop_posting();
        self.pending_io_event(IoTerminalEvent::Disconnected)
    }

    pub(in crate::v2::engine) fn record_cm_failure(
        &mut self,
        error: Error,
    ) -> Option<PendingIoEvent> {
        self.stop_posting();
        if self.close_result.is_none() {
            self.close_result = Some(MemoizedTerminalResult::from_error(error.clone()));
        }
        self.publish_close_result();
        self.pending_io_event(IoTerminalEvent::Terminal(error))
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn mark_cm_failure(
        &mut self,
        error: Error,
    ) -> Option<PendingIoEvent> {
        let event = self.record_cm_failure(error);
        self.wake_close();
        event
    }

    pub(in crate::v2::engine) fn mark_cm_failure_into(
        &mut self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Option<PendingIoEvent> {
        let event = self.record_cm_failure(error);
        self.wake_close_into(actions);
        event
    }

    pub(in crate::v2::engine) fn transition_to_error_once(&mut self) -> Result<bool> {
        if self.error_transition_started {
            return Ok(false);
        }
        self.error_transition_started = true;
        self.poster.transition_qp_to_error()?;
        self.error_transition_complete = true;
        Ok(true)
    }

    pub(in crate::v2::engine) fn error_transition_complete(&self) -> bool {
        self.error_transition_complete
    }

    pub(in crate::v2::engine) fn destroy_connection_resources(
        &mut self,
        outstanding_operations: usize,
    ) -> Result<Option<SharedCmId>> {
        if outstanding_operations != 0 {
            return Err(Error::EngineWedged {
                retained_bundles: 1,
                outstanding_operations,
                cq_debt: outstanding_operations,
            });
        }
        self.stop_posting();
        let destroy_qp = !self.qp_destroyed;
        let (cm_id, qp_destroyed) = self.poster.destroy_connection(destroy_qp)?;
        if qp_destroyed {
            self.record_qp_destroyed();
        }
        if !self.qp_destroyed {
            return Err(Error::InvalidConfig(
                "connection resources lost QP ownership without a destruction boundary".into(),
            ));
        }
        Ok(cm_id)
    }

    pub(in crate::v2::engine) fn destroy_qp_for_session(&mut self) -> Result<QpDestroyStatus> {
        self.stop_posting();
        if self.qp_destroyed {
            return Ok(QpDestroyStatus::AlreadyDestroyed);
        }
        match self.poster.destroy_qp() {
            Ok(true) => {
                if self.record_qp_destroyed() {
                    Ok(QpDestroyStatus::DestroyedNow)
                } else {
                    Ok(QpDestroyStatus::AlreadyDestroyed)
                }
            }
            Ok(false) => {
                if self.qp_destroyed {
                    Ok(QpDestroyStatus::AlreadyDestroyed)
                } else {
                    Err(Error::InvalidConfig(
                        "QP ownership disappeared before its destruction boundary was recorded"
                            .into(),
                    ))
                }
            }
            Err(_) if self.qp_destroyed => Ok(QpDestroyStatus::AlreadyDestroyed),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn wake_close(&self) {
        self.close.notify_waiters();
    }

    pub(in crate::v2::engine) fn close_operation_scan_slot(&self) -> usize {
        self.close_operation_scan_slot
    }

    pub(in crate::v2::engine) fn update_close_operation_scan(
        &mut self,
        next: usize,
        complete: bool,
    ) {
        self.close_operation_scan_slot = next;
        self.close_operation_scan_complete = complete;
    }

    pub(in crate::v2::engine) fn close_operation_scan_complete(&self) -> bool {
        self.close_operation_scan_complete
    }

    pub(in crate::v2::engine) fn quarantine_operation_scan_slot(&self) -> usize {
        self.quarantine_operation_scan_slot
    }

    pub(in crate::v2::engine) fn update_quarantine_operation_scan(
        &mut self,
        next: usize,
        complete: bool,
    ) {
        self.quarantine_operation_scan_slot = next;
        self.quarantine_operation_scan_complete = complete;
    }

    pub(in crate::v2::engine) fn quarantine_operation_scan_complete(&self) -> bool {
        self.quarantine_operation_scan_complete
    }

    pub(in crate::v2::engine) fn store_qp_reclamation_proof(&mut self, proof: QpDestructionProof) {
        let previous = self.qp_reclamation_proof.replace(proof);
        assert!(
            previous.is_none(),
            "connection can retain only its one minted QP destruction proof"
        );
    }

    pub(in crate::v2::engine) fn take_qp_reclamation_proof(
        &mut self,
    ) -> Option<QpDestructionProof> {
        self.qp_reclamation_proof.take()
    }

    pub(in crate::v2::engine) fn wake_close_into(
        &self,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.close.notify_waiters_into(actions);
    }

    pub(in crate::v2::engine) fn begin_quarantine(
        &mut self,
        io_core: &mut super::super::io_core::IoState,
    ) -> Option<IoQuarantineReport> {
        io_core.begin_connection_quarantine(&self.io, &mut self.io_ledger)
    }

    pub(in crate::v2::engine) fn publish_quarantine_into(
        &mut self,
        outstanding_operations: usize,
        cq_debt: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Option<PendingIoEvent> {
        let error = Error::ConnectionQuarantined {
            outstanding_operations,
            cq_debt,
        };
        if self.close_result.is_none() {
            self.close_result = Some(MemoizedTerminalResult::from_error(error.clone()));
        }
        self.publish_close_result();
        self.close.notify_waiters_into(actions);
        self.pending_io_event(IoTerminalEvent::Terminal(error))
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn publish_destroy_quarantine(
        &mut self,
        error: &Error,
        before_publish: impl FnOnce(),
    ) -> (bool, Option<PendingIoEvent>) {
        let newly_published = !self
            .close_result
            .as_ref()
            .is_some_and(MemoizedTerminalResult::is_connection_quarantined);
        if newly_published {
            before_publish();
            let published = Error::ConnectionDestroyQuarantined {
                cause: error.to_string(),
            };
            self.close_result = Some(MemoizedTerminalResult::from_error(published.clone()));
            self.publish_close_result();
            let event = self.pending_io_event(IoTerminalEvent::Terminal(published));
            self.close.notify_waiters();
            return (true, event);
        }
        self.close.notify_waiters();
        (newly_published, None)
    }

    pub(in crate::v2::engine) fn publish_destroy_quarantine_into(
        &mut self,
        error: &Error,
        before_publish: impl FnOnce(),
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> (bool, Option<PendingIoEvent>) {
        let newly_published = !self
            .close_result
            .as_ref()
            .is_some_and(MemoizedTerminalResult::is_connection_quarantined);
        if newly_published {
            before_publish();
            let published = Error::ConnectionDestroyQuarantined {
                cause: error.to_string(),
            };
            self.close_result = Some(MemoizedTerminalResult::from_error(published.clone()));
            self.publish_close_result();
            let event = self.pending_io_event(IoTerminalEvent::Terminal(published));
            self.close.notify_waiters_into(actions);
            return (true, event);
        }
        self.close.notify_waiters_into(actions);
        (false, None)
    }

    pub(in crate::v2::engine) fn retain_bundle_for_engine_failure(
        &self,
        accepted_count: usize,
    ) -> bool {
        accepted_count != 0
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn finish_retirement(&mut self) -> Option<PendingIoEvent> {
        if self.close_result.is_none() {
            self.close_result = Some(MemoizedTerminalResult::success());
        }
        self.publish_close_result();
        self.close.mark_retired();
        self.close.notify_waiters();
        self.pending_io_event(IoTerminalEvent::Closed(Ok(())))
    }

    pub(in crate::v2::engine) fn finish_retirement_into(
        &mut self,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Option<PendingIoEvent> {
        if self.close_result.is_none() {
            self.close_result = Some(MemoizedTerminalResult::success());
        }
        self.publish_close_result();
        self.close.mark_retired();
        self.close.notify_waiters_into(actions);
        self.pending_io_event(IoTerminalEvent::Closed(Ok(())))
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn close_outcome(&self) -> Option<MemoizedTerminalResult> {
        self.close_result.clone()
    }

    pub(in crate::v2::engine) fn operation_close_error(&self) -> Error {
        self.close_result
            .as_ref()
            .and_then(MemoizedTerminalResult::error)
            .unwrap_or(Error::TransportClosed)
    }

    pub(in crate::v2::engine) fn begin_close(&mut self) {
        self.stop_posting();
        if let Some(reservation) = self.admission.as_mut() {
            reservation.mark_draining();
        }
    }

    pub(in crate::v2::engine) fn mark_drained_once(&mut self) -> bool {
        if self.drained_recorded {
            false
        } else {
            self.drained_recorded = true;
            true
        }
    }

    pub(in crate::v2::engine) fn rollback_draining_count(&mut self) {
        if let Some(reservation) = self.admission.as_mut() {
            reservation.rollback_draining();
        }
    }

    pub(in crate::v2::engine) fn mark_reservation_quarantined(&mut self) {
        if let Some(reservation) = self.admission.as_mut() {
            reservation.mark_quarantined();
        }
    }

    pub(in crate::v2::engine) fn recover_reservation_quarantine(&mut self) {
        if let Some(reservation) = self.admission.as_mut() {
            reservation.recover_quarantine(!self.qp_destroyed);
        }
    }

    fn record_qp_destroyed(&mut self) -> bool {
        let first = !self.qp_destroyed;
        self.qp_destroyed = true;
        if first && let Some(reservation) = self.admission.as_mut() {
            reservation.mark_qp_destroyed();
        }
        first
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn retain_setup_rollback_mr(&mut self, mr: Mr) {
        let previous = self.retained_setup_rollback_mr.replace(mr);
        assert!(
            previous.is_none(),
            "setup rollback retains at most one test MR"
        );
    }

    fn publish_close_result(&self) {
        let Some(result) = self.close_result.as_ref() else {
            return;
        };
        let mut observer = lock_unpoison(&self.close.outcome);
        if observer.is_none() || result.is_connection_quarantined() {
            *observer = Some(result.clone());
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn close_result_for_test(&self) -> Option<MemoizedTerminalResult> {
        self.close_result.clone()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn drained_for_test(&self) -> bool {
        self.drained_recorded
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn resource_owner_identity_for_test(&self) -> usize {
        self.poster.owner_identity()
    }
}

#[derive(Debug)]
pub(in crate::v2::engine) struct ConnectionReservation {
    _permit: OwnedSemaphorePermit,
    state: ReservationState,
    qp_counted: bool,
}

impl ConnectionReservation {
    pub(in crate::v2::engine) fn new(permit: OwnedSemaphorePermit) -> Self {
        Self {
            _permit: permit,
            state: ReservationState::Establishing,
            qp_counted: false,
        }
    }

    pub(in crate::v2::engine) fn contribute(&self, counts: &mut ConnectionStateCountSnapshot) {
        match self.state {
            ReservationState::Establishing => counts.establishing += 1,
            ReservationState::Established => counts.established += 1,
            ReservationState::Draining => counts.draining += 1,
            ReservationState::QuarantinedEstablishing
            | ReservationState::QuarantinedEstablished
            | ReservationState::QuarantinedDraining => counts.quarantined_bundles += 1,
        }
        if matches!(self.state, ReservationState::QuarantinedDraining) {
            counts.draining += 1;
        }
        if self.qp_counted {
            counts.registered_live_qps += 1;
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn state(&self) -> ReservationState {
        self.state
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum ReservationState {
    Establishing,
    Established,
    Draining,
    QuarantinedEstablishing,
    QuarantinedEstablished,
    QuarantinedDraining,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::v2::engine) struct ConnectionStateCountSnapshot {
    pub(in crate::v2::engine) live: usize,
    pub(in crate::v2::engine) establishing: usize,
    pub(in crate::v2::engine) established: usize,
    pub(in crate::v2::engine) draining: usize,
    pub(in crate::v2::engine) registered_live_qps: usize,
    pub(in crate::v2::engine) quarantined_bundles: usize,
}

impl ConnectionReservation {
    fn mark_registered(&mut self) {
        if self.state != ReservationState::Establishing {
            return;
        }
        self.state = ReservationState::Established;
        self.qp_counted = true;
    }

    fn mark_draining(&mut self) {
        match self.state {
            ReservationState::Established => {
                self.state = ReservationState::Draining;
            }
            ReservationState::QuarantinedEstablished => {
                self.state = ReservationState::QuarantinedDraining;
            }
            ReservationState::Establishing
            | ReservationState::QuarantinedEstablishing
            | ReservationState::Draining
            | ReservationState::QuarantinedDraining => {}
        }
    }

    fn rollback_draining(&mut self) {
        match self.state {
            ReservationState::Draining => {
                self.state = ReservationState::Established;
            }
            ReservationState::QuarantinedDraining => {
                self.state = ReservationState::QuarantinedEstablished;
            }
            ReservationState::Establishing
            | ReservationState::QuarantinedEstablishing
            | ReservationState::Established
            | ReservationState::QuarantinedEstablished => {}
        }
    }

    fn mark_quarantined(&mut self) {
        match self.state {
            ReservationState::Establishing => {
                self.state = ReservationState::QuarantinedEstablishing;
            }
            ReservationState::Established => {
                self.state = ReservationState::QuarantinedEstablished;
            }
            ReservationState::Draining => {
                self.state = ReservationState::QuarantinedDraining;
            }
            ReservationState::QuarantinedEstablishing
            | ReservationState::QuarantinedEstablished
            | ReservationState::QuarantinedDraining => {}
        }
        self.qp_counted = false;
    }

    fn recover_quarantine(&mut self, qp_is_live: bool) {
        match self.state {
            ReservationState::QuarantinedEstablished => {
                self.state = ReservationState::Established;
                self.qp_counted = qp_is_live;
            }
            ReservationState::QuarantinedDraining => {
                self.state = ReservationState::Draining;
                self.qp_counted = qp_is_live;
            }
            ReservationState::Establishing
            | ReservationState::QuarantinedEstablishing
            | ReservationState::Established
            | ReservationState::Draining => {}
        }
    }

    fn mark_qp_destroyed(&mut self) {
        if !self.qp_counted {
            return;
        }
        self.qp_counted = false;
    }
}

impl ConnectionReservation {
    pub(in crate::v2::engine) fn retain_setup_quarantine(&mut self) -> bool {
        let newly_quarantined = !matches!(
            self.state,
            ReservationState::QuarantinedEstablishing
                | ReservationState::QuarantinedEstablished
                | ReservationState::QuarantinedDraining
        );
        self.mark_quarantined();
        newly_quarantined
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum ConnectionCmRoute {
    Outbound(u64),
    Inbound(u64),
}

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) trait TestConnectionProvider: Send + Sync {
    fn qp_num(&self) -> u32;
    fn capabilities(&self) -> Option<QpCapabilities>;
    fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome>;
    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome>;
    fn to_error(&self) -> Result<()>;
    /// Returns true only when this call successfully takes and destroys the
    /// owned QP. A failure must retain the QP and return its error.
    fn destroy_qp(&self) -> Result<bool>;
    fn destroy_connection(&self, destroy_qp: bool) -> Result<(Option<SharedCmId>, bool)> {
        Ok((
            None,
            if destroy_qp {
                self.destroy_qp()?
            } else {
                false
            },
        ))
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect(&self) -> Result<()>;
    #[cfg(any(test, feature = "test-hooks"))]
    fn fail_next_qp_destroy(&self) -> Result<()> {
        Err(Error::InvalidConfig(
            "QP destroy-failure injection is unavailable for this poster".into(),
        ))
    }
}

pub(in crate::v2::engine) enum ConnectionPoster {
    #[cfg(any(test, feature = "test-hooks"))]
    Shared(Arc<dyn TestConnectionProvider>),
    Verbs(VerbsConnectionResources),
}

#[cfg(any(test, feature = "test-hooks"))]
impl<T> From<Arc<T>> for ConnectionPoster
where
    T: TestConnectionProvider + 'static,
{
    fn from(poster: Arc<T>) -> Self {
        Self::Shared(poster)
    }
}

#[cfg(any(test, feature = "test-hooks"))]
impl From<Arc<dyn TestConnectionProvider>> for ConnectionPoster {
    fn from(poster: Arc<dyn TestConnectionProvider>) -> Self {
        Self::Shared(poster)
    }
}

impl From<VerbsConnectionResources> for ConnectionPoster {
    fn from(resources: VerbsConnectionResources) -> Self {
        Self::Verbs(resources)
    }
}

impl ConnectionPoster {
    fn capabilities(&self) -> Option<QpCapabilities> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.capabilities(),
            Self::Verbs(resources) => Some(resources.capabilities),
        }
    }

    fn transition_qp_to_error(&mut self) -> Result<()> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.to_error(),
            Self::Verbs(resources) => resources.transition_qp_to_error_owned(),
        }
    }

    fn destroy_qp(&mut self) -> Result<bool> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.destroy_qp(),
            Self::Verbs(resources) => resources.destroy_qp_owned(),
        }
    }

    fn destroy_connection(&mut self, destroy_qp: bool) -> Result<(Option<SharedCmId>, bool)> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.destroy_connection(destroy_qp),
            Self::Verbs(resources) => resources.destroy_connection_owned(destroy_qp),
        }
    }

    fn connect(&self, param: &ConnParam) -> Result<()> {
        match self {
            Self::Verbs(resources) => resources.connect(param),
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(_) => Err(Error::InvalidConfig(
                "synthetic connection cannot initiate RDMA-CM connect".into(),
            )),
        }
    }

    fn accept(&self, param: &ConnParam) -> Result<()> {
        match self {
            Self::Verbs(resources) => resources.accept(param),
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(_) => Err(Error::InvalidConfig(
                "synthetic connection cannot initiate RDMA-CM accept".into(),
            )),
        }
    }

    fn reject(&self) -> Result<()> {
        match self {
            Self::Verbs(resources) => resources.reject(),
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(_) => Err(Error::InvalidConfig(
                "synthetic connection cannot reject an RDMA-CM child".into(),
            )),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect_for_test(&self) -> Result<()> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.disconnect(),
            Self::Verbs(resources) => resources.disconnect_owned(),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn fail_next_qp_destroy(&self) -> Result<()> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.fail_next_qp_destroy(),
            Self::Verbs(resources) => resources.fail_next_qp_destroy_owned(),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn uses_engine_resources(&self) -> bool {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(_) => false,
            Self::Verbs(_) => true,
        }
    }

    #[cfg(test)]
    fn owner_identity(&self) -> usize {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => Arc::as_ptr(poster) as *const () as usize,
            Self::Verbs(resources) => resources.qp_num as usize,
        }
    }
}

impl ConnectionPoster {
    pub(in crate::v2::engine) fn qp_num(&self) -> u32 {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.qp_num(),
            Self::Verbs(resources) => resources.qp_num,
        }
    }

    pub(in crate::v2::engine) fn post_send(
        &self,
        batch: &mut PreparedSendBatch,
    ) -> Result<BatchPostOutcome> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.post_send(batch),
            Self::Verbs(resources) => resources.post_send_owned(batch),
        }
    }

    pub(in crate::v2::engine) fn post_recv(
        &self,
        batch: &mut PreparedRecvBatch,
    ) -> Result<BatchPostOutcome> {
        match self {
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Shared(poster) => poster.post_recv(batch),
            Self::Verbs(resources) => resources.post_recv_owned(batch),
        }
    }
}

pub(in crate::v2::engine) struct VerbsConnectionResources {
    qp: Option<Qp>,
    qp_num: u32,
    capabilities: QpCapabilities,
    cm_owner: Option<ConnectionCmOwner>,
}

pub(in crate::v2::engine) struct SharedCmId {
    cm_id: Option<CmId>,
    channel: Option<Arc<EventChannel>>,
}

impl SharedCmId {
    pub(in crate::v2::engine) fn new(cm_id: CmId, channel: Arc<EventChannel>) -> Self {
        Self {
            cm_id: Some(cm_id),
            channel: Some(channel),
        }
    }

    pub(in crate::v2::engine) fn try_destroy(mut self) -> std::result::Result<(), (Self, Error)> {
        let cm_id = self
            .cm_id
            .take()
            .expect("shared CM ID is destroyed exactly once");
        match cm_id.try_destroy() {
            Ok(()) => {
                self.channel.take();
                Ok(())
            }
            Err((cm_id, error)) => {
                self.cm_id = Some(cm_id);
                Err((self, Error::from_v1(error)))
            }
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn from_raw_for_ownership_test(
        raw: *mut rdma_io_sys::rdmacm::rdma_cm_id,
    ) -> Self {
        Self {
            cm_id: Some(unsafe { CmId::from_raw(raw, true) }),
            channel: None,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn disarm_destroy_for_test(&mut self) {
        let mut cm_id = self.cm_id.take().expect("test CM owner remains present");
        cm_id.disarm_destroy_for_test();
    }

    pub(in crate::v2::engine) fn install_context_token(&mut self, route: u64) -> Result<()> {
        self.cm_id
            .as_mut()
            .expect("shared CM ID remains live until driver destruction")
            .install_context_token(route)
            .map_err(Error::from_v1)
    }
}

impl Deref for SharedCmId {
    type Target = CmId;

    fn deref(&self) -> &Self::Target {
        self.cm_id
            .as_ref()
            .expect("shared CM ID remains live until driver destruction")
    }
}

impl Drop for SharedCmId {
    fn drop(&mut self) {
        let Some(cm_id) = self.cm_id.take() else {
            return;
        };
        // Without the sole driver there is no safe way to prove that every
        // event referencing this ID was acknowledged. Retain it instead.
        let channel = self
            .channel
            .take()
            .expect("a live shared CM ID retains its event channel");
        fallback_cm_quarantine()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(RetainedCmId {
                _cm_id: cm_id,
                _channel: channel,
            });
    }
}

struct RetainedCmId {
    _cm_id: CmId,
    _channel: Arc<EventChannel>,
}

fn fallback_cm_quarantine() -> &'static Mutex<Vec<RetainedCmId>> {
    static IDS: OnceLock<Mutex<Vec<RetainedCmId>>> = OnceLock::new();
    IDS.get_or_init(|| Mutex::new(Vec::new()))
}

impl VerbsConnectionResources {
    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn new(qp: Qp, cm_owner: crate::async_cm::AsyncCmId) -> Self {
        let qp_num = qp.qp_num();
        let capabilities = qp.capabilities();
        Self {
            qp: Some(qp),
            qp_num,
            capabilities,
            cm_owner: Some(ConnectionCmOwner::External { _cm_id: cm_owner }),
        }
    }

    pub(in crate::v2::engine) fn new_shared(qp: Qp, cm_id: SharedCmId) -> Self {
        let qp_num = qp.qp_num();
        let capabilities = qp.capabilities();
        Self {
            qp: Some(qp),
            qp_num,
            capabilities,
            cm_owner: Some(ConnectionCmOwner::Shared { cm_id }),
        }
    }

    pub(in crate::v2::engine) fn connect(&self, param: &ConnParam) -> Result<()> {
        match self.cm_owner.as_ref() {
            Some(ConnectionCmOwner::Shared { cm_id, .. }) => {
                cm_id.connect(param).map_err(Error::from_v1)
            }
            #[cfg(any(test, feature = "test-hooks"))]
            Some(ConnectionCmOwner::External { .. }) => Err(Error::InvalidConfig(
                "external CM owner cannot initiate an engine connection".into(),
            )),
            None => Err(Error::TransportClosed),
        }
    }

    pub(in crate::v2::engine) fn reject(&self) -> Result<()> {
        match self.cm_owner.as_ref() {
            Some(ConnectionCmOwner::Shared { cm_id, .. }) => {
                cm_id.reject(&[]).map_err(Error::from_v1)
            }
            #[cfg(any(test, feature = "test-hooks"))]
            Some(ConnectionCmOwner::External { .. }) => Err(Error::InvalidConfig(
                "external CM owner cannot reject an engine connection".into(),
            )),
            None => Err(Error::TransportClosed),
        }
    }

    pub(in crate::v2::engine) fn accept(&self, param: &ConnParam) -> Result<()> {
        match self.cm_owner.as_ref() {
            Some(ConnectionCmOwner::Shared { cm_id, .. }) => {
                cm_id.accept(param).map_err(Error::from_v1)
            }
            #[cfg(any(test, feature = "test-hooks"))]
            Some(ConnectionCmOwner::External { .. }) => Err(Error::InvalidConfig(
                "external CM owner cannot accept an engine connection".into(),
            )),
            None => Err(Error::TransportClosed),
        }
    }
}

enum ConnectionCmOwner {
    Shared {
        cm_id: SharedCmId,
    },
    #[cfg(any(test, feature = "test-hooks"))]
    External {
        _cm_id: crate::async_cm::AsyncCmId,
    },
}

impl Drop for VerbsConnectionResources {
    fn drop(&mut self) {
        let cm_owner = self.cm_owner.take();
        let Some(cm_owner) = cm_owner else {
            return;
        };
        // The driver removes the owner only after the accepted set reaches
        // zero. Any other drop path must retain the complete live bundle.
        let qp = self.qp.take();
        fallback_verbs_quarantine()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(RetainedVerbsConnectionResources {
                _qp: qp,
                _cm_owner: cm_owner,
            });
    }
}

struct RetainedVerbsConnectionResources {
    _qp: Option<Qp>,
    _cm_owner: ConnectionCmOwner,
}

fn fallback_verbs_quarantine() -> &'static Mutex<Vec<RetainedVerbsConnectionResources>> {
    static RESOURCES: OnceLock<Mutex<Vec<RetainedVerbsConnectionResources>>> = OnceLock::new();
    RESOURCES.get_or_init(|| Mutex::new(Vec::new()))
}

impl VerbsConnectionResources {
    fn post_send_owned(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        self.qp
            .as_ref()
            .ok_or(Error::TransportClosed)
            .map(|qp| qp.post_send_batch(batch))
    }

    fn post_recv_owned(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        self.qp
            .as_ref()
            .ok_or(Error::TransportClosed)
            .map(|qp| qp.post_recv_batch(batch))
    }

    fn transition_qp_to_error_owned(&mut self) -> Result<()> {
        match self.qp.as_ref() {
            Some(qp) => qp.to_error(),
            None => Ok(()),
        }
    }

    fn destroy_qp_owned(&mut self) -> Result<bool> {
        let Some(owned) = self.qp.take() else {
            return Ok(false);
        };
        match owned.try_destroy() {
            Ok(()) => Ok(true),
            Err((owned, error)) => {
                self.qp = Some(owned);
                Err(error)
            }
        }
    }

    fn destroy_connection_owned(&mut self, destroy_qp: bool) -> Result<(Option<SharedCmId>, bool)> {
        let qp_destroyed = if destroy_qp {
            self.destroy_qp_owned()?
        } else {
            false
        };
        let cm_id = match self.cm_owner.take() {
            Some(ConnectionCmOwner::Shared { cm_id }) => Some(cm_id),
            #[cfg(any(test, feature = "test-hooks"))]
            Some(ConnectionCmOwner::External { _cm_id }) => {
                drop(_cm_id);
                None
            }
            None => None,
        };
        Ok((cm_id, qp_destroyed))
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect_owned(&self) -> Result<()> {
        match self.cm_owner.as_ref() {
            Some(ConnectionCmOwner::Shared { cm_id, .. }) => {
                cm_id.disconnect().map_err(Error::from_v1)
            }
            #[cfg(any(test, feature = "test-hooks"))]
            Some(ConnectionCmOwner::External { _cm_id }) => {
                _cm_id.disconnect().map_err(Error::from_v1)
            }
            None => Err(Error::TransportClosed),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn fail_next_qp_destroy_owned(&self) -> Result<()> {
        let qp = self.qp.as_ref().ok_or(Error::TransportClosed)?;
        qp.fail_next_destroy();
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn install_connection(
    manager: &SessionManager,
    connections: &mut ConnectionRegistry,
    poster: impl Into<ConnectionPoster>,
    config: RdmaConnectionConfig,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
) -> Result<RdmaConnection> {
    manager.validate_connection_config(&config)?;
    let (admission, reservation) = reserve_connection(manager, connections)?;
    let connection = install_reserved_connection(
        manager,
        connections,
        None,
        poster,
        config,
        local_addr,
        peer_addr,
        reservation,
    );
    drop(admission);
    match connection {
        Ok(connection) => Ok(connection),
        Err(failure) => {
            let (error, resources) = failure.into_parts();
            #[cfg(any(test, feature = "test-hooks"))]
            if let FailedConnectionInstallResources::Registered(token) = resources {
                let _ = connections.release_unindexed(token);
            }
            Err(error)
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
pub(in crate::v2::engine) fn install_admitted_test_connection(
    manager: &SessionManager,
    connections: &mut ConnectionRegistry,
    poster: impl Into<ConnectionPoster>,
    config: RdmaConnectionConfig,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    reservation: ConnectionReservation,
) -> Result<RdmaConnection> {
    match install_reserved_connection(
        manager,
        connections,
        None,
        poster,
        config,
        local_addr,
        peer_addr,
        reservation,
    ) {
        Ok(connection) => Ok(connection),
        Err(failure) => {
            let (error, resources) = failure.into_parts();
            if let FailedConnectionInstallResources::Registered(token) = resources {
                let _ = connections.release_unindexed(token);
            }
            Err(error)
        }
    }
}

pub(in crate::v2::engine) fn reserve_connection<'a>(
    manager: &'a SessionManager,
    connections: &ConnectionRegistry,
) -> Result<(RwLockReadGuard<'a, ()>, ConnectionReservation)> {
    let admission = read_unpoison(&manager.frontend.admission);
    if let Some(error) = manager.admission_error() {
        return Err(error);
    }
    let reservation = connections.try_reserve().ok_or(Error::CapacityExhausted)?;
    Ok((admission, reservation))
}

#[allow(
    clippy::too_many_arguments,
    clippy::result_large_err,
    reason = "failed installation returns the complete provider/resource bundle for exact cleanup"
)]
pub(in crate::v2::engine) fn install_reserved_connection(
    manager: &SessionManager,
    connections: &mut ConnectionRegistry,
    route_token: Option<ConnectionToken>,
    poster: impl Into<ConnectionPoster>,
    config: RdmaConnectionConfig,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    reservation: ConnectionReservation,
) -> std::result::Result<RdmaConnection, ConnectionInstallFailure> {
    let poster = poster.into();
    if let Err(error) = manager.validate_connection_config(&config) {
        return Err(ConnectionInstallFailure::unregistered(
            error,
            poster,
            reservation,
        ));
    }
    if let Some(capabilities) = poster.capabilities()
        && let Err(error) = capabilities.require(&config)
    {
        return Err(ConnectionInstallFailure::unregistered(
            error,
            poster,
            reservation,
        ));
    }
    let qp_num = poster.qp_num();
    if let Err(error) = connections.validate_qp_registration(qp_num) {
        return Err(ConnectionInstallFailure::unregistered(
            error,
            poster,
            reservation,
        ));
    }
    let (token, _snapshot) = if let Some(token) = route_token {
        let state = ConnectionState::new(
            token,
            poster,
            config,
            local_addr,
            peer_addr,
            Some(reservation),
        );
        let snapshot = match connections.attach_registered(token, qp_num, state) {
            Ok(snapshot) => snapshot,
            Err(failure) => {
                return Err(ConnectionInstallFailure {
                    error: failure.error,
                    resources: FailedConnectionInstallResources::Detached(
                        failure
                            .retained
                            .expect("failed attachment retains the connection state")
                            .1,
                    ),
                });
            }
        };
        (token, snapshot)
    } else {
        let mut pending = Some((poster, reservation));
        let registration = connections.register(qp_num, |token| {
            let (poster, reservation) = pending
                .take()
                .expect("connection registration factory runs exactly once");
            ConnectionState::new(
                token,
                poster,
                config,
                local_addr,
                peer_addr,
                Some(reservation),
            )
        });
        match registration {
            Ok(registration) => registration,
            Err(failure) => {
                if let Some((_token, state)) = failure.retained {
                    return Err(ConnectionInstallFailure {
                        error: failure.error,
                        resources: FailedConnectionInstallResources::Detached(state),
                    });
                }
                let (poster, reservation) = pending
                    .take()
                    .expect("failed allocation retains unconsumed connection resources");
                return Err(ConnectionInstallFailure::unregistered(
                    failure.error,
                    poster,
                    reservation,
                ));
            }
        }
    };
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(failure) = manager.take_setup_rollback_failure() {
        let injected = connections.with_connection_mut(token, |state| {
            state.retain_setup_rollback_mr(failure.retained_mr);
            state.poster.fail_next_qp_destroy()
        });
        let Some(injected) = injected else {
            return Err(ConnectionInstallFailure {
                error: Error::InvalidConfig(
                    "setup rollback injection lost its connection entry".into(),
                ),
                resources: FailedConnectionInstallResources::Unrecoverable,
            });
        };
        if !connections.detach_qp_index(token, qp_num) {
            return Err(ConnectionInstallFailure {
                error: Error::InvalidConfig(
                    "setup rollback injection lost its QP registration".into(),
                ),
                resources: FailedConnectionInstallResources::Registered(token),
            });
        }
        if let Err(injection_error) = injected {
            return Err(ConnectionInstallFailure {
                error: injection_error,
                resources: FailedConnectionInstallResources::Registered(token),
            });
        }
        return Err(ConnectionInstallFailure {
            error: failure.error,
            resources: FailedConnectionInstallResources::Registered(token),
        });
    }
    let route = connections.connection_route(token);
    connections
        .with_connection(token, |state| {
            RdmaConnection::from_registered(manager, state, route)
        })
        .ok_or_else(|| ConnectionInstallFailure {
            error: Error::InvalidConfig(
                "registered connection disappeared before frontend construction".into(),
            ),
            resources: FailedConnectionInstallResources::Unrecoverable,
        })
}

pub(in crate::v2::engine) struct ConnectionInstallFailure {
    error: Error,
    resources: FailedConnectionInstallResources,
}

impl std::fmt::Debug for ConnectionInstallFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionInstallFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "variants retain complete value-owned provider bundles for exact rollback"
)]
pub(in crate::v2::engine) enum FailedConnectionInstallResources {
    Unregistered {
        poster: ConnectionPoster,
        reservation: ConnectionReservation,
    },
    #[cfg(any(test, feature = "test-hooks"))]
    Registered(ConnectionToken),
    Detached(ConnectionState),
    Unrecoverable,
}

impl ConnectionInstallFailure {
    fn unregistered(
        error: Error,
        poster: ConnectionPoster,
        reservation: ConnectionReservation,
    ) -> Self {
        Self {
            error,
            resources: FailedConnectionInstallResources::Unregistered {
                poster,
                reservation,
            },
        }
    }

    pub(in crate::v2::engine) fn into_parts(self) -> (Error, FailedConnectionInstallResources) {
        (self.error, self.resources)
    }
}

impl FailedConnectionInstallResources {
    pub(in crate::v2::engine) fn reject_for_session(
        &self,
        connections: &ConnectionRegistry,
    ) -> Result<()> {
        #[cfg(not(any(test, feature = "test-hooks")))]
        let _ = connections;
        match self {
            Self::Unregistered { poster, .. } => poster.reject(),
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Registered(token) => connections
                .with_connection(*token, ConnectionState::reject)
                .ok_or(Error::TransportClosed)?,
            Self::Detached(connection) => connection.reject(),
            Self::Unrecoverable => Ok(()),
        }
    }

    pub(in crate::v2::engine) fn destroy_for_session(
        &mut self,
        connections: &mut ConnectionRegistry,
    ) -> Result<(Option<SharedCmId>, bool)> {
        #[cfg(not(any(test, feature = "test-hooks")))]
        let _ = connections;
        match self {
            Self::Unregistered { poster, .. } => poster.destroy_connection(true),
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Registered(token) => connections
                .with_connection_mut(*token, |connection| {
                    connection.destroy_connection_resources(0)
                })
                .ok_or(Error::TransportClosed)?
                .map(|cm_id| (cm_id, true)),
            Self::Detached(connection) => connection
                .destroy_connection_resources(0)
                .map(|cm_id| (cm_id, true)),
            Self::Unrecoverable => Ok((None, false)),
        }
    }
}

mod qp {
    use super::*;

    pub(in crate::v2::engine) trait QpCapabilitiesExt {
        fn require(&self, config: &RdmaConnectionConfig) -> Result<()>;
    }

    impl QpCapabilitiesExt for QpCapabilities {
        fn require(&self, config: &RdmaConnectionConfig) -> Result<()> {
            let required = [
                (
                    "maximum send WRs",
                    self.max_send_wr as usize,
                    config.max_send_wr,
                ),
                (
                    "maximum receive WRs",
                    self.max_recv_wr as usize,
                    config.max_recv_wr,
                ),
                (
                    "maximum send SGEs",
                    self.max_send_sge as usize,
                    config.max_send_sge,
                ),
                (
                    "maximum receive SGEs",
                    self.max_recv_sge as usize,
                    config.max_recv_sge,
                ),
            ];
            for (name, actual, requested) in required {
                if actual < requested {
                    return Err(Error::InvalidConfig(format!(
                        "provider returned {name} {actual}, below requested {requested}"
                    )));
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
