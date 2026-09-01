//! Engine-owned low-level connection frontend.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard};

use tokio::sync::Notify;

use self::qp::QpCapabilitiesExt;
use super::diagnostics::RdmaConnectionDiagnostics;
use super::lifecycle::MemoizedTerminalResult;
use super::operation::RdmaOperation;
use super::registry::{
    ConnectionToken, OperationToken, lock_unpoison, read_unpoison, write_unpoison,
};
use super::{ConnectionReadyWork, EngineShared, RdmaConnectionConfig};
use crate::cm::{CmId, ConnParam, EventChannel};
use crate::v2::error::{Error, Result};
use crate::v2::mr::{AccessIntent, Mr, RemoteMr};
use crate::v2::qp::{BatchPostOutcome, Qp, QpCapabilities};
use crate::wc::WorkCompletion;
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

    pub(super) fn registry_slot(&self) -> u32 {
        self.slot
    }

    pub(super) fn registration_generation(&self) -> u32 {
        self.generation
    }
}

/// Engine-owned low-level RDMA connection.
///
/// The connection exposes owned operation futures but no raw PD, QP, CQ, CM,
/// or independently pollable completion-driver handle. Establishment posts
/// zero initial receives.
pub struct RdmaConnection {
    pub(crate) shared: Arc<EngineShared>,
    pub(crate) state: Arc<ConnectionState>,
}

impl Clone for RdmaConnection {
    fn clone(&self) -> Self {
        self.state.frontend_count.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
            state: Arc::clone(&self.state),
        }
    }
}

impl RdmaConnection {
    pub(crate) fn from_state(shared: Arc<EngineShared>, state: Arc<ConnectionState>) -> Self {
        state.frontend_count.fetch_add(1, Ordering::Relaxed);
        Self { shared, state }
    }

    /// Register owned memory against this engine's shared protection domain.
    ///
    /// The returned MR remains owned by the caller until it is submitted in an
    /// [`RdmaOperation`]. Length must be nonzero and fit the provider ABI.
    pub fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        self.shared.register_memory(len, access)
    }

    /// Create a two-sided SEND operation submitted on first poll.
    ///
    /// The optional `(offset, length)` selects a checked MR range. Awaiting the
    /// future returns `(Result<Completion>, Option<Mr>)`.
    pub fn send(&self, mr: Mr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.state),
            OperationKind::Send,
            mr,
            None,
            range,
        )
    }

    /// Create a two-sided RECV operation submitted on first poll.
    pub fn recv(&self, mr: Mr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.state),
            OperationKind::Recv,
            mr,
            None,
            range,
        )
    }

    /// Create an RDMA WRITE operation submitted on first poll.
    pub fn write(&self, mr: Mr, remote: RemoteMr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.state),
            OperationKind::Write,
            mr,
            Some(remote),
            range,
        )
    }

    /// Create an RDMA READ operation submitted on first poll.
    pub fn read(&self, mr: Mr, remote: RemoteMr, range: Option<(usize, usize)>) -> RdmaOperation {
        RdmaOperation::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.state),
            OperationKind::Read,
            mr,
            Some(remote),
            range,
        )
    }

    /// Return the local socket address reported by RDMA-CM.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.state
            .local_addr
            .ok_or_else(|| Error::InvalidConfig("connection local address is unavailable".into()))
    }

    /// Return the peer socket address reported by RDMA-CM.
    pub fn peer_addr(&self) -> Result<SocketAddr> {
        self.state
            .peer_addr
            .ok_or_else(|| Error::InvalidConfig("connection peer address is unavailable".into()))
    }

    /// Return the opaque current connection identity and exact `qp_num`.
    pub fn identity(&self) -> RdmaConnectionIdentity {
        self.state.identity()
    }

    pub(crate) fn attach_ready_work(&self, work: Arc<dyn ConnectionReadyWork>) -> Result<()> {
        self.state.attach_ready_work(work)?;
        self.shared.schedule_deadline(
            super::DeadlineKind::MessageHello,
            self.state.token.encode(),
            self.shared.config.hello_deadline,
        );
        self.shared.publish_connection_ready(&self.state);
        Ok(())
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
        self.shared.begin_connection_close(&self.state);
        loop {
            let notified = self.state.close_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.state.close_outcome() {
                return outcome.into_result();
            }
            if let Some(outcome) = self.shared.outcome() {
                return outcome.into_result();
            }
            notified.await;
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn transition_to_error_for_test(&self) -> Result<()> {
        self.state.poster.to_error()
    }

    #[cfg(test)]
    pub(super) fn into_state_without_close_for_test(self) -> Arc<ConnectionState> {
        let state = Arc::clone(&self.state);
        state.frontend_count.fetch_add(1, Ordering::Relaxed);
        drop(self);
        let previous = state.frontend_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert_eq!(previous, 1);
        state
    }
}

impl Drop for RdmaConnection {
    fn drop(&mut self) {
        let previous = self.state.frontend_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "connection frontend count must be positive");
        if previous == 1 && !self.state.is_retired() {
            self.shared.begin_connection_close(&self.state);
        }
    }
}

pub(crate) struct ConnectionState {
    pub(super) token: ConnectionToken,
    qp_num: u32,
    config: RdmaConnectionConfig,
    pub(super) poster: Arc<dyn WorkRequestPoster>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    posting_open: AtomicBool,
    // Nested lifecycle synchronization always follows:
    // EngineShared::admission -> lifecycle_gate -> posting_gate.
    // ready_work is only held long enough to clone/install its Arc and is
    // never held while invoking ConnectionReadyWork.
    posting_gate: RwLock<()>,
    lifecycle_gate: Mutex<()>,
    local_credits: Mutex<LocalCredits>,
    accepted: Mutex<HashSet<AcceptedWrIdentity>>,
    completions: Mutex<VecDeque<WorkCompletion>>,
    ready_work: Mutex<Option<Arc<dyn ConnectionReadyWork>>>,
    ready_published: AtomicBool,
    close_started: AtomicBool,
    close_outcome: Mutex<Option<MemoizedTerminalResult>>,
    close_notify: Notify,
    quarantined: AtomicBool,
    error_transition_started: AtomicBool,
    error_transition_complete: AtomicBool,
    qp_destruction_boundary: AtomicBool,
    frontend_count: AtomicUsize,
    retirement_requested: AtomicBool,
    retirement_started: AtomicBool,
    retirement_quarantined: AtomicBool,
    drained_recorded: AtomicBool,
    retired: AtomicBool,
    admission: Mutex<Option<ConnectionReservation>>,
    cm_route: Option<ConnectionCmRoute>,
    #[cfg(any(test, feature = "test-hooks"))]
    retained_setup_rollback_mr: Mutex<Option<Mr>>,
}

impl ConnectionState {
    pub(super) fn new(
        token: ConnectionToken,
        poster: Arc<dyn WorkRequestPoster>,
        config: RdmaConnectionConfig,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
        mut admission: Option<ConnectionReservation>,
        cm_route: Option<ConnectionCmRoute>,
    ) -> Self {
        if let Some(reservation) = admission.as_mut() {
            reservation.mark_registered();
        }
        Self {
            token,
            qp_num: poster.qp_num(),
            config,
            poster,
            local_addr,
            peer_addr,
            posting_open: AtomicBool::new(true),
            posting_gate: RwLock::new(()),
            lifecycle_gate: Mutex::new(()),
            local_credits: Mutex::new(LocalCredits::default()),
            accepted: Mutex::new(HashSet::new()),
            completions: Mutex::new(VecDeque::new()),
            ready_work: Mutex::new(None),
            ready_published: AtomicBool::new(false),
            close_started: AtomicBool::new(false),
            close_outcome: Mutex::new(None),
            close_notify: Notify::new(),
            quarantined: AtomicBool::new(false),
            error_transition_started: AtomicBool::new(false),
            error_transition_complete: AtomicBool::new(false),
            qp_destruction_boundary: AtomicBool::new(false),
            frontend_count: AtomicUsize::new(1),
            retirement_requested: AtomicBool::new(false),
            retirement_started: AtomicBool::new(false),
            retirement_quarantined: AtomicBool::new(false),
            drained_recorded: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            admission: Mutex::new(admission),
            cm_route,
            #[cfg(any(test, feature = "test-hooks"))]
            retained_setup_rollback_mr: Mutex::new(None),
        }
    }

    pub(super) fn identity(&self) -> RdmaConnectionIdentity {
        RdmaConnectionIdentity {
            slot: self.token.slot,
            generation: self.token.generation,
            qp_num: self.qp_num,
        }
    }

    pub(super) fn qp_num(&self) -> u32 {
        self.qp_num
    }

    pub(super) fn reserve_local(&self, direction: Direction) -> Result<()> {
        if !self.posting_open.load(Ordering::Acquire) {
            return Err(Error::TransportClosed);
        }
        let mut credits = lock_unpoison(&self.local_credits);
        let (used, maximum) = match direction {
            Direction::Send => (&mut credits.send, self.config.max_send_wr),
            Direction::Recv => (&mut credits.recv, self.config.max_recv_wr),
        };
        if *used >= maximum {
            return Err(Error::CapacityExhausted);
        }
        *used += 1;
        Ok(())
    }

    pub(super) fn begin_posting(&self) -> Result<RwLockReadGuard<'_, ()>> {
        let guard = read_unpoison(&self.posting_gate);
        if !self.posting_open.load(Ordering::Acquire) {
            return Err(Error::TransportClosed);
        }
        Ok(guard)
    }

    pub(super) fn release_local(&self, direction: Direction) {
        let mut credits = lock_unpoison(&self.local_credits);
        let used = match direction {
            Direction::Send => &mut credits.send,
            Direction::Recv => &mut credits.recv,
        };
        *used = used.saturating_sub(1);
    }

    pub(super) fn add_accepted(&self, token: OperationToken) {
        lock_unpoison(&self.accepted).insert(AcceptedWrIdentity {
            connection: self.token,
            qp_num: self.qp_num,
            operation: token,
        });
    }

    pub(super) fn remove_accepted(&self, token: OperationToken) -> bool {
        let removed = lock_unpoison(&self.accepted).remove(&AcceptedWrIdentity {
            connection: self.token,
            qp_num: self.qp_num,
            operation: token,
        });
        if removed {
            self.close_notify.notify_waiters();
        }
        removed
    }

    pub(super) fn accepted_tokens(&self) -> Vec<OperationToken> {
        lock_unpoison(&self.accepted)
            .iter()
            .map(|identity| identity.operation)
            .collect()
    }

    pub(super) fn accepted_count(&self) -> usize {
        lock_unpoison(&self.accepted).len()
    }

    pub(super) fn diagnostics(&self, quarantined: bool) -> RdmaConnectionDiagnostics {
        RdmaConnectionDiagnostics {
            identity: self.identity(),
            accepted_outstanding_operations: self.accepted_count(),
            draining: self.close_started(),
            quarantined,
        }
    }

    pub(super) fn enqueue_completion(&self, completion: WorkCompletion) {
        lock_unpoison(&self.completions).push_back(completion);
    }

    pub(super) fn pop_completion(&self) -> Option<WorkCompletion> {
        lock_unpoison(&self.completions).pop_front()
    }

    pub(super) fn has_completion_work(&self) -> bool {
        !lock_unpoison(&self.completions).is_empty()
    }

    pub(super) fn attach_ready_work(&self, work: Arc<dyn ConnectionReadyWork>) -> Result<()> {
        let mut current = lock_unpoison(&self.ready_work);
        if current.is_some() {
            return Err(Error::InvalidConfig(
                "connection already has attached ready work".into(),
            ));
        }
        *current = Some(work);
        Ok(())
    }

    pub(super) fn process_ready_work(&self, budget: usize) -> usize {
        self.ready_work().map_or(0, |work| work.process(budget))
    }

    pub(super) fn has_attached_work(&self) -> bool {
        self.ready_work().is_some_and(|work| work.has_work())
    }

    pub(super) fn handle_message_deadline(&self) {
        if let Some(work) = self.ready_work() {
            work.deadline_expired();
        }
    }

    fn ready_work(&self) -> Option<Arc<dyn ConnectionReadyWork>> {
        lock_unpoison(&self.ready_work).clone()
    }

    pub(super) fn mark_ready_published(&self) -> bool {
        !self.ready_published.swap(true, Ordering::AcqRel)
    }

    pub(super) fn clear_ready_published(&self) {
        self.ready_published.store(false, Ordering::Release);
    }

    pub(super) fn stop_posting(&self) {
        let _posting = write_unpoison(&self.posting_gate);
        self.posting_open.store(false, Ordering::Release);
    }

    pub(super) fn lock_lifecycle(&self) -> MutexGuard<'_, ()> {
        lock_unpoison(&self.lifecycle_gate)
    }

    pub(super) fn finalize_engine(&self, outcome: &MemoizedTerminalResult) {
        self.stop_posting();
        let _ = self.transition_to_error_once();
        if let Some(error) = outcome.error() {
            let mut close_outcome = lock_unpoison(&self.close_outcome);
            if close_outcome.is_none() {
                *close_outcome = Some(MemoizedTerminalResult::from_error(error.clone()));
            }
            drop(close_outcome);
            if let Some(work) = self.ready_work() {
                work.terminalize(error);
            }
        }
    }

    pub(super) fn mark_disconnected(&self) {
        self.stop_posting();
        if let Some(work) = self.ready_work() {
            work.disconnected();
        }
    }

    pub(super) fn mark_cm_failure(&self, error: Error) {
        self.stop_posting();
        let mut outcome = lock_unpoison(&self.close_outcome);
        if outcome.is_none() {
            *outcome = Some(MemoizedTerminalResult::from_error(error.clone()));
        }
        drop(outcome);
        if let Some(work) = self.ready_work() {
            work.terminalize(error);
        }
        self.close_notify.notify_waiters();
    }

    pub(super) fn transition_to_error_once(&self) -> Result<bool> {
        if self.error_transition_started.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        self.poster.to_error()?;
        self.error_transition_complete
            .store(true, Ordering::Release);
        Ok(true)
    }

    pub(super) fn error_transition_complete(&self) -> bool {
        self.error_transition_complete.load(Ordering::Acquire)
    }

    pub(super) fn destroy_connection_resources(
        &self,
        _lifecycle: &MutexGuard<'_, ()>,
    ) -> Result<(Option<SharedCmId>, bool)> {
        let outstanding_operations = self.accepted_count();
        if outstanding_operations != 0 {
            return Err(Error::EngineWedged {
                retained_bundles: 1,
                outstanding_operations,
                cq_debt: outstanding_operations,
            });
        }
        self.stop_posting();
        let destroy_qp = !self.qp_destruction_boundary.load(Ordering::Acquire);
        let resources = self.poster.destroy_connection(destroy_qp)?;
        if resources.1 {
            self.mark_qp_destroyed();
        }
        Ok(resources)
    }

    pub(super) fn establish_qp_destruction_boundary(
        &self,
        _lifecycle: &MutexGuard<'_, ()>,
    ) -> Result<bool> {
        self.stop_posting();
        if self.qp_destruction_boundary.load(Ordering::Acquire) {
            return Ok(false);
        }
        match self.poster.destroy_qp() {
            Ok(true) => {
                self.mark_qp_destroyed();
                Ok(true)
            }
            Ok(false) => {
                if self.qp_destruction_boundary.load(Ordering::Acquire) {
                    Ok(false)
                } else {
                    Err(Error::InvalidConfig(
                        "QP ownership disappeared before its destruction boundary was recorded"
                            .into(),
                    ))
                }
            }
            Err(error) => {
                if self.qp_destruction_boundary.load(Ordering::Acquire) {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn disconnect_for_test(&self) -> Result<()> {
        self.poster.disconnect()
    }

    pub(super) fn wake_close(&self) {
        self.close_notify.notify_waiters();
    }

    pub(super) fn begin_quarantine(&self) -> Option<(usize, usize)> {
        let accepted = lock_unpoison(&self.accepted);
        let outstanding = accepted.len();
        if outstanding == 0 {
            return None;
        }
        if self.quarantined.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some((outstanding, outstanding))
    }

    pub(super) fn publish_quarantine(&self, outstanding: usize) {
        let mut outcome = lock_unpoison(&self.close_outcome);
        if outcome.is_none() {
            *outcome = Some(MemoizedTerminalResult::from_error(
                Error::ConnectionQuarantined {
                    outstanding_operations: outstanding,
                    cq_debt: outstanding,
                },
            ));
        }
        drop(outcome);
        self.close_notify.notify_waiters();
    }

    pub(super) fn publish_destroy_quarantine(
        &self,
        error: &Error,
        before_publish: impl FnOnce(),
    ) -> bool {
        self.retirement_quarantined.store(true, Ordering::Release);
        let mut outcome = lock_unpoison(&self.close_outcome);
        let newly_published = !outcome
            .as_ref()
            .is_some_and(MemoizedTerminalResult::is_connection_quarantined);
        if newly_published {
            before_publish();
            *outcome = Some(MemoizedTerminalResult::from_error(
                Error::ConnectionDestroyQuarantined {
                    cause: error.to_string(),
                },
            ));
        }
        drop(outcome);
        self.close_notify.notify_waiters();
        newly_published
    }

    pub(super) fn recover_quarantine(&self) -> bool {
        self.quarantined.swap(false, Ordering::AcqRel)
    }

    pub(super) fn retain_bundle_for_engine_failure(&self) -> bool {
        self.accepted_count() != 0 && !self.quarantined.swap(true, Ordering::AcqRel)
    }

    pub(super) fn finish_retirement(&self) {
        let mut outcome = lock_unpoison(&self.close_outcome);
        if outcome.is_none() {
            *outcome = Some(MemoizedTerminalResult::success());
        }
        drop(outcome);
        self.retired.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
    }

    pub(super) fn fail_retirement(&self, error: Error) {
        self.stop_posting();
        self.release_admission();
        let mut outcome = lock_unpoison(&self.close_outcome);
        if !outcome
            .as_ref()
            .is_some_and(MemoizedTerminalResult::is_connection_quarantined)
        {
            *outcome = Some(MemoizedTerminalResult::from_error(error));
        }
        drop(outcome);
        self.retired.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
    }

    fn close_outcome(&self) -> Option<MemoizedTerminalResult> {
        let outcome = lock_unpoison(&self.close_outcome).clone();
        match outcome {
            Some(ref value) if value.is_connection_quarantined() => outcome,
            Some(_) if self.is_retired() => outcome,
            _ => None,
        }
    }

    pub(super) fn operation_close_error(&self) -> Error {
        lock_unpoison(&self.close_outcome)
            .as_ref()
            .and_then(MemoizedTerminalResult::error)
            .unwrap_or(Error::TransportClosed)
    }

    pub(super) fn close_started(&self) -> bool {
        self.close_started.load(Ordering::Acquire)
    }

    pub(super) fn begin_close(&self) -> bool {
        self.stop_posting();
        let first = !self.close_started.swap(true, Ordering::AcqRel);
        if first && let Some(reservation) = lock_unpoison(&self.admission).as_mut() {
            reservation.mark_draining();
        }
        first
    }

    pub(super) fn try_request_retirement(&self) -> bool {
        !self.retirement_requested.swap(true, Ordering::AcqRel)
    }

    pub(super) fn try_begin_retirement(&self) -> bool {
        if self.retirement_quarantined.load(Ordering::Acquire) {
            return false;
        }
        !self.retirement_started.swap(true, Ordering::AcqRel)
    }

    pub(super) fn retry_retirement(&self) {
        if self.retirement_quarantined.load(Ordering::Acquire) {
            return;
        }
        self.retirement_started.store(false, Ordering::Release);
    }

    pub(super) fn retirement_is_quarantined(&self) -> bool {
        self.retirement_quarantined.load(Ordering::Acquire)
    }

    pub(super) fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    pub(super) fn mark_drained_once(&self) -> bool {
        !self.drained_recorded.swap(true, Ordering::AcqRel)
    }

    pub(super) fn rollback_draining_count(&self) {
        if let Some(reservation) = lock_unpoison(&self.admission).as_mut() {
            reservation.rollback_draining();
        }
    }

    pub(super) fn mark_diagnostic_quarantined(&self) {
        if let Some(reservation) = lock_unpoison(&self.admission).as_mut() {
            reservation.mark_quarantined();
        }
    }

    pub(super) fn mark_diagnostic_recovered(&self) {
        if let Some(reservation) = lock_unpoison(&self.admission).as_mut() {
            reservation.recover_quarantine(!self.qp_destruction_boundary.load(Ordering::Acquire));
        }
    }

    pub(super) fn mark_qp_destroyed(&self) {
        if !self.qp_destruction_boundary.swap(true, Ordering::AcqRel)
            && let Some(reservation) = lock_unpoison(&self.admission).as_mut()
        {
            reservation.mark_qp_destroyed();
        }
    }

    pub(super) fn cm_route(&self) -> Option<ConnectionCmRoute> {
        self.cm_route
    }

    pub(super) fn release_admission(&self) {
        drop(lock_unpoison(&self.admission).take());
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn retain_setup_rollback_mr(&self, mr: Mr) {
        let previous = lock_unpoison(&self.retained_setup_rollback_mr).replace(mr);
        assert!(
            previous.is_none(),
            "setup rollback retains at most one test MR"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct AcceptedWrIdentity {
    pub(super) connection: ConnectionToken,
    pub(super) qp_num: u32,
    pub(super) operation: OperationToken,
}

pub(super) struct ConnectionAdmissionPool {
    capacity: usize,
    counts: Arc<ConnectionStateCounts>,
}

impl ConnectionAdmissionPool {
    pub(super) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            counts: Arc::new(ConnectionStateCounts::default()),
        })
    }

    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<ConnectionReservation> {
        if !self.counts.try_acquire(self.capacity) {
            return None;
        }
        Some(ConnectionReservation {
            counts: Arc::clone(&self.counts),
            state: ReservationState::Establishing,
            qp_counted: false,
        })
    }

    pub(super) fn snapshot(&self) -> ConnectionStateCountSnapshot {
        self.counts.snapshot()
    }

    pub(super) fn clear_retained_quarantine(&self) {
        self.counts.update(|counts| {
            counts.live = counts.live.saturating_sub(1);
            counts.quarantined_bundles = counts.quarantined_bundles.saturating_sub(1);
        });
    }
}

pub(super) struct ConnectionReservation {
    counts: Arc<ConnectionStateCounts>,
    state: ReservationState,
    qp_counted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservationState {
    Establishing,
    Established,
    Draining,
    QuarantinedEstablishing,
    QuarantinedEstablished,
    QuarantinedDraining,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ConnectionStateCountSnapshot {
    pub(super) live: usize,
    pub(super) establishing: usize,
    pub(super) established: usize,
    pub(super) draining: usize,
    pub(super) registered_live_qps: usize,
    pub(super) quarantined_bundles: usize,
}

#[derive(Default)]
struct ConnectionStateCounts {
    writer: Mutex<()>,
    version: AtomicU64,
    live: AtomicUsize,
    establishing: AtomicUsize,
    established: AtomicUsize,
    draining: AtomicUsize,
    registered_live_qps: AtomicUsize,
    quarantined_bundles: AtomicUsize,
}

impl ConnectionStateCounts {
    fn try_acquire(&self, capacity: usize) -> bool {
        self.update(|counts| {
            if counts.live >= capacity {
                return false;
            }
            counts.live += 1;
            counts.establishing += 1;
            true
        })
    }

    fn update<T>(&self, update: impl FnOnce(&mut ConnectionStateCountSnapshot) -> T) -> T {
        let _writer = lock_unpoison(&self.writer);
        let previous = self.version.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(previous & 1, 0, "connection gauge writer must be exclusive");
        let mut counts = ConnectionStateCountSnapshot {
            live: self.live.load(Ordering::Relaxed),
            establishing: self.establishing.load(Ordering::Relaxed),
            established: self.established.load(Ordering::Relaxed),
            draining: self.draining.load(Ordering::Relaxed),
            registered_live_qps: self.registered_live_qps.load(Ordering::Relaxed),
            quarantined_bundles: self.quarantined_bundles.load(Ordering::Relaxed),
        };
        let result = update(&mut counts);
        self.live.store(counts.live, Ordering::Relaxed);
        self.establishing
            .store(counts.establishing, Ordering::Relaxed);
        self.established
            .store(counts.established, Ordering::Relaxed);
        self.draining.store(counts.draining, Ordering::Relaxed);
        self.registered_live_qps
            .store(counts.registered_live_qps, Ordering::Relaxed);
        self.quarantined_bundles
            .store(counts.quarantined_bundles, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Release);
        result
    }

    fn snapshot(&self) -> ConnectionStateCountSnapshot {
        loop {
            let before = self.version.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let counts = ConnectionStateCountSnapshot {
                live: self.live.load(Ordering::Relaxed),
                establishing: self.establishing.load(Ordering::Relaxed),
                established: self.established.load(Ordering::Relaxed),
                draining: self.draining.load(Ordering::Relaxed),
                registered_live_qps: self.registered_live_qps.load(Ordering::Relaxed),
                quarantined_bundles: self.quarantined_bundles.load(Ordering::Relaxed),
            };
            if self.version.load(Ordering::Acquire) == before {
                return counts;
            }
        }
    }
}

impl ConnectionReservation {
    fn mark_registered(&mut self) {
        if self.state != ReservationState::Establishing {
            return;
        }
        self.counts.update(|counts| {
            counts.establishing = counts.establishing.saturating_sub(1);
            counts.established += 1;
            counts.registered_live_qps += 1;
        });
        self.state = ReservationState::Established;
        self.qp_counted = true;
    }

    fn mark_draining(&mut self) {
        match self.state {
            ReservationState::Established => {
                self.counts.update(|counts| {
                    counts.established = counts.established.saturating_sub(1);
                    counts.draining += 1;
                });
                self.state = ReservationState::Draining;
            }
            ReservationState::QuarantinedEstablished => {
                self.counts.update(|counts| counts.draining += 1);
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
                self.counts.update(|counts| {
                    counts.draining = counts.draining.saturating_sub(1);
                    counts.established += 1;
                });
                self.state = ReservationState::Established;
            }
            ReservationState::QuarantinedDraining => {
                self.counts
                    .update(|counts| counts.draining = counts.draining.saturating_sub(1));
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
                self.counts.update(|counts| {
                    counts.establishing = counts.establishing.saturating_sub(1);
                    counts.quarantined_bundles += 1;
                });
                self.state = ReservationState::QuarantinedEstablishing;
            }
            ReservationState::Established => {
                self.counts.update(|counts| {
                    counts.established = counts.established.saturating_sub(1);
                    if self.qp_counted {
                        counts.registered_live_qps = counts.registered_live_qps.saturating_sub(1);
                    }
                    counts.quarantined_bundles += 1;
                });
                self.state = ReservationState::QuarantinedEstablished;
            }
            ReservationState::Draining => {
                if self.qp_counted {
                    self.counts.update(|counts| {
                        counts.registered_live_qps = counts.registered_live_qps.saturating_sub(1);
                        counts.quarantined_bundles += 1;
                    });
                } else {
                    self.counts.update(|counts| counts.quarantined_bundles += 1);
                }
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
                self.counts.update(|counts| {
                    counts.established += 1;
                    if qp_is_live {
                        counts.registered_live_qps += 1;
                    }
                    counts.quarantined_bundles = counts.quarantined_bundles.saturating_sub(1);
                });
                self.state = ReservationState::Established;
                self.qp_counted = qp_is_live;
            }
            ReservationState::QuarantinedDraining => {
                self.counts.update(|counts| {
                    if qp_is_live {
                        counts.registered_live_qps += 1;
                    }
                    counts.quarantined_bundles = counts.quarantined_bundles.saturating_sub(1);
                });
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
        self.counts.update(|counts| {
            counts.registered_live_qps = counts.registered_live_qps.saturating_sub(1);
        });
        self.qp_counted = false;
    }
}

impl Drop for ConnectionReservation {
    fn drop(&mut self) {
        self.counts.update(|counts| {
            match self.state {
                ReservationState::Establishing => {
                    counts.live = counts.live.saturating_sub(1);
                    counts.establishing = counts.establishing.saturating_sub(1);
                }
                ReservationState::Established => {
                    counts.live = counts.live.saturating_sub(1);
                    counts.established = counts.established.saturating_sub(1);
                }
                ReservationState::Draining => {
                    counts.live = counts.live.saturating_sub(1);
                    counts.draining = counts.draining.saturating_sub(1);
                }
                ReservationState::QuarantinedEstablishing
                | ReservationState::QuarantinedEstablished
                | ReservationState::QuarantinedDraining => {
                    // A quarantined reservation pins admission and bundle
                    // diagnostics even if a future owner is dropped.
                }
            }
            if self.qp_counted {
                counts.registered_live_qps = counts.registered_live_qps.saturating_sub(1);
            }
        });
    }
}

impl ConnectionReservation {
    pub(super) fn retain_setup_quarantine(&mut self) -> bool {
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

#[derive(Default)]
struct LocalCredits {
    send: usize,
    recv: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Send,
    Recv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OperationKind {
    Send,
    Recv,
    Write,
    Read,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConnectionCmRoute {
    Outbound(u64),
    Inbound(u64),
}

impl OperationKind {
    pub(super) const fn direction(self) -> Direction {
        match self {
            Self::Recv => Direction::Recv,
            Self::Send | Self::Write | Self::Read => Direction::Send,
        }
    }
}

pub(crate) trait WorkRequestPoster: Send + Sync {
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

pub(super) struct VerbsConnectionResources {
    qp: Mutex<Option<Qp>>,
    qp_num: u32,
    capabilities: QpCapabilities,
    cm_owner: Mutex<Option<ConnectionCmOwner>>,
}

pub(super) struct SharedCmId {
    cm_id: Option<CmId>,
    channel: Option<Arc<EventChannel>>,
}

impl SharedCmId {
    pub(super) fn new(cm_id: CmId, channel: Arc<EventChannel>) -> Self {
        Self {
            cm_id: Some(cm_id),
            channel: Some(channel),
        }
    }

    pub(super) fn destroy(mut self) -> Result<()> {
        let cm_id = self
            .cm_id
            .take()
            .expect("shared CM ID is destroyed exactly once");
        let result = cm_id.destroy().map_err(Error::from_v1);
        self.channel.take();
        result
    }

    pub(super) fn install_context_token(&mut self, route: u64) -> Result<()> {
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
    pub(super) fn new(qp: Qp, cm_owner: crate::async_cm::AsyncCmId) -> Self {
        let qp_num = qp.qp_num();
        let capabilities = qp.capabilities();
        Self {
            qp: Mutex::new(Some(qp)),
            qp_num,
            capabilities,
            cm_owner: Mutex::new(Some(ConnectionCmOwner::External { _cm_id: cm_owner })),
        }
    }

    pub(super) fn new_shared(qp: Qp, cm_id: SharedCmId) -> Self {
        let qp_num = qp.qp_num();
        let capabilities = qp.capabilities();
        Self {
            qp: Mutex::new(Some(qp)),
            qp_num,
            capabilities,
            cm_owner: Mutex::new(Some(ConnectionCmOwner::Shared { cm_id })),
        }
    }

    pub(super) fn connect(&self, param: &ConnParam) -> Result<()> {
        let cm_owner = lock_unpoison(&self.cm_owner);
        match cm_owner.as_ref() {
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

    pub(super) fn reject(&self) -> Result<()> {
        let cm_owner = lock_unpoison(&self.cm_owner);
        match cm_owner.as_ref() {
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

    pub(super) fn accept(&self, param: &ConnParam) -> Result<()> {
        let cm_owner = lock_unpoison(&self.cm_owner);
        match cm_owner.as_ref() {
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
        let cm_owner = self
            .cm_owner
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(cm_owner) = cm_owner else {
            return;
        };
        // The driver removes the owner only after the accepted set reaches
        // zero. Any other drop path must retain the complete live bundle.
        let qp = self
            .qp
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
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

impl WorkRequestPoster for VerbsConnectionResources {
    fn qp_num(&self) -> u32 {
        self.qp_num
    }

    fn capabilities(&self) -> Option<QpCapabilities> {
        Some(self.capabilities)
    }

    fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        let qp = lock_unpoison(&self.qp);
        qp.as_ref()
            .ok_or(Error::TransportClosed)
            .map(|qp| qp.post_send_batch(batch))
    }

    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        let qp = lock_unpoison(&self.qp);
        qp.as_ref()
            .ok_or(Error::TransportClosed)
            .map(|qp| qp.post_recv_batch(batch))
    }

    fn to_error(&self) -> Result<()> {
        let qp = lock_unpoison(&self.qp);
        match qp.as_ref() {
            Some(qp) => qp.to_error(),
            None => Ok(()),
        }
    }

    fn destroy_qp(&self) -> Result<bool> {
        let mut qp = lock_unpoison(&self.qp);
        let Some(owned) = qp.take() else {
            return Ok(false);
        };
        match owned.try_destroy() {
            Ok(()) => Ok(true),
            Err((owned, error)) => {
                *qp = Some(owned);
                Err(error)
            }
        }
    }

    fn destroy_connection(&self, destroy_qp: bool) -> Result<(Option<SharedCmId>, bool)> {
        let qp_destroyed = if destroy_qp {
            self.destroy_qp()?
        } else {
            false
        };
        let cm_id = match lock_unpoison(&self.cm_owner).take() {
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
    fn disconnect(&self) -> Result<()> {
        let cm_owner = lock_unpoison(&self.cm_owner);
        match cm_owner.as_ref() {
            Some(ConnectionCmOwner::Shared { cm_id, .. }) => {
                cm_id.disconnect().map_err(Error::from_v1)
            }
            Some(ConnectionCmOwner::External { _cm_id }) => {
                _cm_id.disconnect().map_err(Error::from_v1)
            }
            None => Err(Error::TransportClosed),
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn fail_next_qp_destroy(&self) -> Result<()> {
        let qp = lock_unpoison(&self.qp);
        let qp = qp.as_ref().ok_or(Error::TransportClosed)?;
        qp.fail_next_destroy();
        Ok(())
    }
}

#[allow(
    dead_code,
    reason = "used by the test-only external-CM connection installer"
)]
pub(crate) fn install_connection(
    shared: &Arc<EngineShared>,
    poster: Arc<dyn WorkRequestPoster>,
    config: RdmaConnectionConfig,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
) -> Result<RdmaConnection> {
    config.validate(&shared.config, shared.provider.as_ref())?;
    let (admission, reservation) = reserve_connection(shared)?;
    let connection = install_reserved_connection(
        shared,
        poster,
        config,
        local_addr,
        peer_addr,
        reservation,
        None,
    );
    drop(admission);
    match connection {
        Ok(connection) => Ok(connection),
        Err(failure) => {
            let (error, resources) = failure.into_parts();
            if let FailedConnectionInstallResources::Registered(connection) = resources {
                let _ = shared.connections.release_unindexed(connection.token);
                connection.release_admission();
            }
            Err(error)
        }
    }
}

pub(super) fn reserve_connection(
    shared: &Arc<EngineShared>,
) -> Result<(RwLockReadGuard<'_, ()>, ConnectionReservation)> {
    let admission = read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        return Err(error);
    }
    let reservation = shared.connection_admission.try_acquire().ok_or_else(|| {
        shared
            .diagnostic_counters
            .connection_capacity_exhausted
            .fetch_add(1, Ordering::Relaxed);
        Error::CapacityExhausted
    })?;
    shared
        .diagnostic_counters
        .connections_admitted
        .fetch_add(1, Ordering::Relaxed);
    Ok((admission, reservation))
}

pub(super) fn install_reserved_connection(
    shared: &Arc<EngineShared>,
    poster: Arc<dyn WorkRequestPoster>,
    config: RdmaConnectionConfig,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    reservation: ConnectionReservation,
    cm_route: Option<ConnectionCmRoute>,
) -> std::result::Result<RdmaConnection, ConnectionInstallFailure> {
    if let Err(error) = config.validate(&shared.config, shared.provider.as_ref()) {
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
    let pending = Arc::new(Mutex::new(Some((poster, reservation))));
    let make_pending = Arc::clone(&pending);
    let registration = shared.connections.register(qp_num, move |token| {
        let (poster, reservation) = lock_unpoison(&make_pending)
            .take()
            .expect("connection registration factory runs exactly once");
        Arc::new(ConnectionState::new(
            token,
            poster,
            config,
            local_addr,
            peer_addr,
            Some(reservation),
            cm_route,
        ))
    });
    let (token, state) = match registration {
        Ok(registration) => registration,
        Err(failure) => {
            if matches!(failure.error, Error::CapacityExhausted) {
                shared
                    .diagnostic_counters
                    .connection_capacity_exhausted
                    .fetch_add(1, Ordering::Relaxed);
            }
            if let Some((_token, state)) = failure.retained {
                return Err(ConnectionInstallFailure {
                    error: failure.error,
                    resources: FailedConnectionInstallResources::Registered(state),
                });
            }
            let (poster, reservation) = lock_unpoison(&pending)
                .take()
                .expect("failed registration retains unconsumed resources");
            return Err(ConnectionInstallFailure::unregistered(
                failure.error,
                poster,
                reservation,
            ));
        }
    };
    #[cfg(not(any(test, feature = "test-hooks")))]
    let _ = token;
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(failure) = shared.test_driver.take_setup_rollback_failure() {
        state.retain_setup_rollback_mr(failure.retained_mr);
        if !shared.connections.detach_qp_index(token, qp_num) {
            return Err(ConnectionInstallFailure {
                error: Error::InvalidConfig(
                    "setup rollback injection lost its QP registration".into(),
                ),
                resources: FailedConnectionInstallResources::Registered(state),
            });
        }
        if let Err(injection_error) = state.poster.fail_next_qp_destroy() {
            return Err(ConnectionInstallFailure {
                error: injection_error,
                resources: FailedConnectionInstallResources::Registered(state),
            });
        }
        return Err(ConnectionInstallFailure {
            error: failure.error,
            resources: FailedConnectionInstallResources::Registered(state),
        });
    }
    Ok(RdmaConnection {
        shared: Arc::clone(shared),
        state,
    })
}

pub(super) struct ConnectionInstallFailure {
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

pub(super) enum FailedConnectionInstallResources {
    Unregistered {
        poster: Arc<dyn WorkRequestPoster>,
        reservation: ConnectionReservation,
    },
    Registered(Arc<ConnectionState>),
}

impl ConnectionInstallFailure {
    fn unregistered(
        error: Error,
        poster: Arc<dyn WorkRequestPoster>,
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

    pub(super) fn into_parts(self) -> (Error, FailedConnectionInstallResources) {
        (self.error, self.resources)
    }
}

mod qp {
    use super::*;

    pub(super) trait QpCapabilitiesExt {
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
mod tests {
    use super::*;
    use crate::wr::{RecvWr, SendWr, WrOpcode};
    use std::sync::Weak;

    struct TestPoster;

    impl WorkRequestPoster for TestPoster {
        fn qp_num(&self) -> u32 {
            1
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn to_error(&self) -> Result<()> {
            Ok(())
        }

        fn destroy_qp(&self) -> Result<bool> {
            Ok(false)
        }

        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    struct LockCheckingReadyWork {
        connection: Weak<ConnectionState>,
        calls: AtomicUsize,
    }

    impl LockCheckingReadyWork {
        fn check_ready_work_unlocked(&self) {
            let connection = self.connection.upgrade().expect("connection state");
            assert!(
                connection.ready_work.try_lock().is_ok(),
                "ConnectionReadyWork callback ran while ready_work was locked"
            );
            self.calls.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl ConnectionReadyWork for LockCheckingReadyWork {
        fn process(&self, _budget: usize) -> usize {
            self.check_ready_work_unlocked();
            1
        }

        fn has_work(&self) -> bool {
            self.check_ready_work_unlocked();
            false
        }

        fn deadline_expired(&self) {
            self.check_ready_work_unlocked();
        }

        fn disconnected(&self) {
            self.check_ready_work_unlocked();
        }

        fn terminalize(&self, _error: Error) {
            self.check_ready_work_unlocked();
        }
    }

    #[test]
    fn ready_work_mutex_is_released_before_every_callback() {
        let connection = Arc::new(ConnectionState::new(
            ConnectionToken {
                slot: 0,
                generation: 1,
            },
            Arc::new(TestPoster),
            RdmaConnectionConfig::default(),
            None,
            None,
            None,
            None,
        ));
        let work = Arc::new(LockCheckingReadyWork {
            connection: Arc::downgrade(&connection),
            calls: AtomicUsize::new(0),
        });
        connection
            .attach_ready_work(Arc::clone(&work) as Arc<dyn ConnectionReadyWork>)
            .unwrap();

        assert_eq!(connection.process_ready_work(1), 1);
        assert!(!connection.has_attached_work());
        connection.handle_message_deadline();
        connection.mark_disconnected();
        connection.mark_cm_failure(Error::DriverShutdown);
        assert_eq!(work.calls.load(Ordering::Acquire), 5);
    }

    #[test]
    fn returned_qp_capabilities_must_not_be_reduced() {
        let config = RdmaConnectionConfig::default();
        QpCapabilities {
            max_send_wr: 19,
            max_recv_wr: 34,
            max_send_sge: 1,
            max_recv_sge: 1,
        }
        .require(&config)
        .unwrap();
        assert!(
            QpCapabilities {
                max_send_wr: 18,
                max_recv_wr: 34,
                max_send_sge: 1,
                max_recv_sge: 1,
            }
            .require(&config)
            .is_err()
        );
    }

    #[test]
    fn public_connection_surface_is_send_sync_without_raw_accessors() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<RdmaConnection>();
        let _: fn(&RdmaConnection) -> RdmaConnectionIdentity = RdmaConnection::identity;
        let _: fn(&RdmaConnection) -> Result<SocketAddr> = RdmaConnection::local_addr;
        let _: fn(&RdmaConnection) -> Result<SocketAddr> = RdmaConnection::peer_addr;
    }

    #[test]
    fn memoized_close_failure_preserves_its_typed_error() {
        let outcome =
            MemoizedTerminalResult::from_error(Error::InvalidConfig("typed close failure".into()));
        for result in [outcome.clone().into_result(), outcome.into_result()] {
            assert!(
                matches!(result, Err(Error::InvalidConfig(ref message)) if message == "typed close failure")
            );
        }
    }

    #[test]
    fn destroy_quarantine_publishes_callback_and_outcome_once() {
        let connection = ConnectionState::new(
            ConnectionToken {
                slot: 0,
                generation: 1,
            },
            Arc::new(TestPoster),
            RdmaConnectionConfig::default(),
            None,
            None,
            None,
            None,
        );
        let publications = AtomicUsize::new(0);

        assert!(connection.publish_destroy_quarantine(
            &Error::InvalidConfig("first destroy failure".into()),
            || {
                publications.fetch_add(1, Ordering::Relaxed);
            },
        ));
        assert!(!connection.publish_destroy_quarantine(
            &Error::InvalidConfig("repeated destroy failure".into()),
            || {
                publications.fetch_add(1, Ordering::Relaxed);
            },
        ));

        assert_eq!(publications.load(Ordering::Relaxed), 1);
        assert!(matches!(
            connection.close_outcome().unwrap().into_result(),
            Err(Error::ConnectionDestroyQuarantined { cause })
                if cause.contains("first destroy failure")
        ));
    }

    #[test]
    fn connection_state_counts_follow_exact_reservation_transitions() {
        let pool = ConnectionAdmissionPool::new(1);
        let mut reservation = pool.try_acquire().expect("reservation");
        assert_eq!(
            pool.snapshot(),
            ConnectionStateCountSnapshot {
                live: 1,
                establishing: 1,
                ..ConnectionStateCountSnapshot::default()
            }
        );

        reservation.mark_registered();
        assert_eq!(
            pool.snapshot(),
            ConnectionStateCountSnapshot {
                live: 1,
                established: 1,
                registered_live_qps: 1,
                ..ConnectionStateCountSnapshot::default()
            }
        );

        reservation.mark_draining();
        reservation.mark_quarantined();
        assert_eq!(
            pool.snapshot(),
            ConnectionStateCountSnapshot {
                live: 1,
                draining: 1,
                quarantined_bundles: 1,
                ..ConnectionStateCountSnapshot::default()
            }
        );

        reservation.recover_quarantine(true);
        reservation.mark_qp_destroyed();
        assert_eq!(
            pool.snapshot(),
            ConnectionStateCountSnapshot {
                live: 1,
                draining: 1,
                ..ConnectionStateCountSnapshot::default()
            }
        );

        drop(reservation);
        assert_eq!(pool.snapshot(), ConnectionStateCountSnapshot::default());
    }

    #[test]
    fn dropped_setup_quarantine_keeps_admission_and_bundle_pinned() {
        let pool = ConnectionAdmissionPool::new(1);
        let mut reservation = pool.try_acquire().expect("reservation");

        assert!(reservation.retain_setup_quarantine());
        assert!(!reservation.retain_setup_quarantine());
        drop(reservation);

        assert_eq!(
            pool.snapshot(),
            ConnectionStateCountSnapshot {
                live: 1,
                quarantined_bundles: 1,
                ..ConnectionStateCountSnapshot::default()
            }
        );
        assert!(pool.try_acquire().is_none());
    }

    #[test]
    fn clearing_dropped_setup_quarantine_releases_live_and_bundle_gauges_together() {
        let pool = ConnectionAdmissionPool::new(1);
        let mut reservation = pool.try_acquire().unwrap();
        assert!(reservation.retain_setup_quarantine());
        drop(reservation);

        pool.clear_retained_quarantine();

        assert_eq!(pool.snapshot(), ConnectionStateCountSnapshot::default());
        assert!(pool.try_acquire().is_some());
    }

    #[test]
    fn destroy_with_accepted_work_fails_closed_without_destroying() {
        struct DestroyPoster(AtomicUsize);

        impl WorkRequestPoster for DestroyPoster {
            fn qp_num(&self) -> u32 {
                7
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
                self.0.fetch_add(1, Ordering::AcqRel);
                Ok(true)
            }

            fn disconnect(&self) -> Result<()> {
                Ok(())
            }
        }

        let poster = Arc::new(DestroyPoster(AtomicUsize::new(0)));
        let connection = ConnectionState::new(
            ConnectionToken {
                slot: 1,
                generation: 1,
            },
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
            None,
            None,
        );
        connection.add_accepted(OperationToken {
            slot: 2,
            generation: 1,
        });

        let lifecycle = connection.lock_lifecycle();
        let error = match connection.destroy_connection_resources(&lifecycle) {
            Ok(_) => panic!("accepted work must prevent connection destruction"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            Error::EngineWedged {
                retained_bundles: 1,
                outstanding_operations: 1,
                cq_debt: 1
            }
        ));
        assert_eq!(poster.0.load(Ordering::Acquire), 0);
    }

    #[test]
    fn missing_qp_returns_typed_post_errors() {
        let resources = VerbsConnectionResources {
            qp: Mutex::new(None),
            qp_num: 9,
            capabilities: QpCapabilities {
                max_send_wr: 1,
                max_recv_wr: 1,
                max_send_sge: 1,
                max_recv_sge: 1,
            },
            cm_owner: Mutex::new(None),
        };
        let mut send =
            PreparedSendBatch::new(vec![SendWr::new(1, WrOpcode::Send)]).expect("send batch");
        let mut recv = PreparedRecvBatch::new(vec![RecvWr::new(2)]).expect("recv batch");

        assert!(matches!(
            resources.post_send(&mut send),
            Err(Error::TransportClosed)
        ));
        assert!(matches!(
            resources.post_recv(&mut recv),
            Err(Error::TransportClosed)
        ));
    }
}
