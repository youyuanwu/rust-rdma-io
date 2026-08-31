//! Engine-owned low-level connection frontend.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard};

use tokio::sync::Notify;

use self::qp::QpCapabilitiesExt;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RdmaConnectionIdentity {
    pub slot: u32,
    pub generation: u32,
    pub qp_num: u32,
}

/// Engine-owned low-level RDMA connection.
///
/// The connection exposes owned operation futures but no raw PD, QP, CQ, CM,
/// or independently pollable completion-driver handle.
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
    pub fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
        self.shared.register_memory(len, access)
    }

    /// Submit a two-sided SEND on first poll.
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

    /// Submit a two-sided RECV on first poll.
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

    /// Submit an RDMA WRITE on first poll.
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

    /// Submit an RDMA READ on first poll.
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

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.state
            .local_addr
            .ok_or_else(|| Error::InvalidConfig("connection local address is unavailable".into()))
    }

    pub fn peer_addr(&self) -> Result<SocketAddr> {
        self.state
            .peer_addr
            .ok_or_else(|| Error::InvalidConfig("connection peer address is unavailable".into()))
    }

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

    pub(crate) fn publish_ready_work(&self) {
        self.shared.publish_connection_ready(&self.state);
    }

    /// Stop new posting and wait for the exact accepted set to drain.
    ///
    /// A successful close retires the CM route and connection registry
    /// generation, destroys the QP before its CM ID, and returns aggregate
    /// admission once. A drain timeout retains every unresolved operation, MR,
    /// registration, and CQ credit in the quarantined connection bundle. The
    /// default deadline is five seconds, and its typed quarantine result is
    /// permanently memoized even if exact late CQEs later recover the bundle.
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
    posting_gate: RwLock<()>,
    lifecycle_gate: Mutex<()>,
    local_credits: Mutex<LocalCredits>,
    accepted: Mutex<HashSet<AcceptedWrIdentity>>,
    completions: Mutex<VecDeque<WorkCompletion>>,
    ready_work: Mutex<Option<Arc<dyn ConnectionReadyWork>>>,
    ready_published: AtomicBool,
    close_started: AtomicBool,
    close_outcome: Mutex<Option<CloseOutcome>>,
    close_notify: Notify,
    quarantined: AtomicBool,
    error_transition_started: AtomicBool,
    error_transition_complete: AtomicBool,
    frontend_count: AtomicUsize,
    retirement_requested: AtomicBool,
    retirement_started: AtomicBool,
    drained_recorded: AtomicBool,
    draining_counted: AtomicBool,
    retired: AtomicBool,
    admission: Mutex<Option<ConnectionReservation>>,
    cm_route: Option<ConnectionCmRoute>,
}

impl ConnectionState {
    pub(super) fn new(
        token: ConnectionToken,
        poster: Arc<dyn WorkRequestPoster>,
        config: RdmaConnectionConfig,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
        admission: Option<ConnectionReservation>,
        cm_route: Option<ConnectionCmRoute>,
    ) -> Self {
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
            frontend_count: AtomicUsize::new(1),
            retirement_requested: AtomicBool::new(false),
            retirement_started: AtomicBool::new(false),
            drained_recorded: AtomicBool::new(false),
            draining_counted: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            admission: Mutex::new(admission),
            cm_route,
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
            Direction::Send => (&mut credits.send, self.config.max_send_wr_value()),
            Direction::Recv => (&mut credits.recv, self.config.max_recv_wr_value()),
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
        lock_unpoison(&self.ready_work)
            .as_ref()
            .map_or(0, |work| work.process(budget))
    }

    pub(super) fn has_attached_work(&self) -> bool {
        lock_unpoison(&self.ready_work)
            .as_ref()
            .is_some_and(|work| work.has_work())
    }

    pub(super) fn handle_message_deadline(&self) {
        if let Some(work) = lock_unpoison(&self.ready_work).as_ref() {
            work.deadline_expired();
        }
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

    pub(super) fn finalize_engine(&self, outcome: &super::EngineOutcome) {
        self.stop_posting();
        let _ = self.transition_to_error_once();
        if let Some(error) = outcome.clone().into_result().err() {
            let mut close_outcome = lock_unpoison(&self.close_outcome);
            if close_outcome.is_none() {
                *close_outcome = Some(CloseOutcome::Failed(error.clone()));
            }
            drop(close_outcome);
            if let Some(work) = lock_unpoison(&self.ready_work).as_ref() {
                work.terminalize(error);
            }
        }
    }

    pub(super) fn mark_disconnected(&self) {
        self.stop_posting();
        if let Some(work) = lock_unpoison(&self.ready_work).as_ref() {
            work.disconnected();
        }
    }

    pub(super) fn mark_cm_failure(&self, error: Error) {
        self.stop_posting();
        let mut outcome = lock_unpoison(&self.close_outcome);
        if outcome.is_none() {
            *outcome = Some(CloseOutcome::Failed(error.clone()));
        }
        drop(outcome);
        if let Some(work) = lock_unpoison(&self.ready_work).as_ref() {
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

    pub(super) fn destroy_connection_resources(&self) -> (Option<SharedCmId>, bool) {
        debug_assert_eq!(self.accepted_count(), 0);
        self.stop_posting();
        self.poster.destroy_connection()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn disconnect_for_test(&self) -> Result<()> {
        self.poster.disconnect()
    }

    pub(super) fn wake_close(&self) {
        self.close_notify.notify_waiters();
    }

    pub(super) fn apply_drain_deadline(&self) -> Option<(usize, usize)> {
        let accepted = lock_unpoison(&self.accepted);
        let outstanding = accepted.len();
        if outstanding == 0 {
            return None;
        }
        if self.quarantined.swap(true, Ordering::AcqRel) {
            return None;
        }
        let mut outcome = lock_unpoison(&self.close_outcome);
        if outcome.is_none() {
            *outcome = Some(CloseOutcome::Quarantined {
                outstanding,
                cq_debt: outstanding,
            });
        }
        drop(accepted);
        drop(outcome);
        self.close_notify.notify_waiters();
        Some((outstanding, outstanding))
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
            *outcome = Some(CloseOutcome::Success);
        }
        drop(outcome);
        self.retired.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
    }

    pub(super) fn fail_retirement(&self, error: Error) {
        self.stop_posting();
        self.release_admission();
        let mut outcome = lock_unpoison(&self.close_outcome);
        if !matches!(*outcome, Some(CloseOutcome::Quarantined { .. })) {
            *outcome = Some(CloseOutcome::Failed(error));
        }
        drop(outcome);
        self.retired.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
    }

    fn close_outcome(&self) -> Option<CloseOutcome> {
        let outcome = lock_unpoison(&self.close_outcome).clone();
        match outcome {
            Some(CloseOutcome::Quarantined { .. }) => outcome,
            Some(_) if self.is_retired() => outcome,
            _ => None,
        }
    }

    pub(super) fn operation_close_error(&self) -> Error {
        match lock_unpoison(&self.close_outcome).clone() {
            Some(CloseOutcome::Failed(error)) => error,
            Some(CloseOutcome::Quarantined {
                outstanding,
                cq_debt,
            }) => Error::ConnectionQuarantined {
                outstanding_operations: outstanding,
                cq_debt,
            },
            Some(CloseOutcome::Success) | None => Error::TransportClosed,
        }
    }

    pub(super) fn close_started(&self) -> bool {
        self.close_started.load(Ordering::Acquire)
    }

    pub(super) fn begin_close(&self) -> bool {
        self.stop_posting();
        !self.close_started.swap(true, Ordering::AcqRel)
    }

    pub(super) fn try_request_retirement(&self) -> bool {
        !self.retirement_requested.swap(true, Ordering::AcqRel)
    }

    pub(super) fn try_begin_retirement(&self) -> bool {
        !self.retirement_started.swap(true, Ordering::AcqRel)
    }

    pub(super) fn retry_retirement(&self) {
        self.retirement_started.store(false, Ordering::Release);
    }

    pub(super) fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    pub(super) fn mark_drained_once(&self) -> bool {
        !self.drained_recorded.swap(true, Ordering::AcqRel)
    }

    pub(super) fn mark_draining_counted(&self) -> bool {
        !self.draining_counted.swap(true, Ordering::AcqRel)
    }

    pub(super) fn take_draining_counted(&self) -> bool {
        self.draining_counted.swap(false, Ordering::AcqRel)
    }

    pub(super) fn cm_route(&self) -> Option<ConnectionCmRoute> {
        self.cm_route
    }

    pub(super) fn release_admission(&self) {
        drop(lock_unpoison(&self.admission).take());
    }
}

#[derive(Clone)]
enum CloseOutcome {
    Success,
    Quarantined { outstanding: usize, cq_debt: usize },
    Failed(Error),
}

impl CloseOutcome {
    fn into_result(self) -> Result<()> {
        match self {
            Self::Success => Ok(()),
            Self::Quarantined {
                outstanding,
                cq_debt,
            } => Err(Error::ConnectionQuarantined {
                outstanding_operations: outstanding,
                cq_debt,
            }),
            Self::Failed(error) => Err(error),
        }
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
    used: AtomicUsize,
}

impl ConnectionAdmissionPool {
    pub(super) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            used: AtomicUsize::new(0),
        })
    }

    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<ConnectionReservation> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            if used >= self.capacity {
                return None;
            }
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ConnectionReservation {
                        pool: Arc::clone(self),
                    });
                }
                Err(observed) => used = observed,
            }
        }
    }

    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

pub(super) struct ConnectionReservation {
    pool: Arc<ConnectionAdmissionPool>,
}

impl Drop for ConnectionReservation {
    fn drop(&mut self) {
        let previous = self.pool.used.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "connection admission must be reserved");
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
    fn post_send(&self, batch: &mut PreparedSendBatch) -> BatchPostOutcome;
    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> BatchPostOutcome;
    fn to_error(&self) -> Result<()>;
    /// Returns true only when this call takes and destroys the owned QP.
    fn destroy_qp(&self) -> bool;
    fn destroy_connection(&self) -> (Option<SharedCmId>, bool) {
        (None, self.destroy_qp())
    }
    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect(&self) -> Result<()>;
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
        let result = cm_id.destroy().map_err(Error::from);
        self.channel.take();
        result
    }

    pub(super) fn install_context_token(&mut self, route: u64) -> Result<()> {
        self.cm_id
            .as_mut()
            .expect("shared CM ID remains live until driver destruction")
            .install_context_token(route)
            .map_err(Error::from)
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
                cm_id.connect(param).map_err(Error::from)
            }
            #[cfg(any(test, feature = "test-hooks"))]
            Some(ConnectionCmOwner::External { .. }) => Err(Error::InvalidConfig(
                "external CM owner cannot initiate an engine connection".into(),
            )),
            None => Err(Error::TransportClosed),
        }
    }

    pub(super) fn accept(&self, param: &ConnParam) -> Result<()> {
        let cm_owner = lock_unpoison(&self.cm_owner);
        match cm_owner.as_ref() {
            Some(ConnectionCmOwner::Shared { cm_id, .. }) => {
                cm_id.accept(param).map_err(Error::from)
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

    fn post_send(&self, batch: &mut PreparedSendBatch) -> BatchPostOutcome {
        let qp = lock_unpoison(&self.qp);
        qp.as_ref()
            .expect("posting is stopped before engine QP destruction")
            .post_send_batch(batch)
    }

    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> BatchPostOutcome {
        let qp = lock_unpoison(&self.qp);
        qp.as_ref()
            .expect("posting is stopped before engine QP destruction")
            .post_recv_batch(batch)
    }

    fn to_error(&self) -> Result<()> {
        let qp = lock_unpoison(&self.qp);
        match qp.as_ref() {
            Some(qp) => qp.to_error(),
            None => Ok(()),
        }
    }

    fn destroy_qp(&self) -> bool {
        let qp = lock_unpoison(&self.qp).take();
        if let Some(qp) = qp {
            qp.destroy();
            true
        } else {
            false
        }
    }

    fn destroy_connection(&self) -> (Option<SharedCmId>, bool) {
        let qp_destroyed = self.destroy_qp();
        let cm_id = match lock_unpoison(&self.cm_owner).take() {
            Some(ConnectionCmOwner::Shared { cm_id }) => Some(cm_id),
            #[cfg(any(test, feature = "test-hooks"))]
            Some(ConnectionCmOwner::External { _cm_id }) => {
                drop(_cm_id);
                None
            }
            None => None,
        };
        (cm_id, qp_destroyed)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect(&self) -> Result<()> {
        let cm_owner = lock_unpoison(&self.cm_owner);
        match cm_owner.as_ref() {
            Some(ConnectionCmOwner::Shared { cm_id, .. }) => {
                cm_id.disconnect().map_err(Error::from)
            }
            Some(ConnectionCmOwner::External { _cm_id }) => {
                _cm_id.disconnect().map_err(Error::from)
            }
            None => Err(Error::TransportClosed),
        }
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
    connection
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
) -> Result<RdmaConnection> {
    config.validate(&shared.config, shared.provider.as_ref())?;
    if let Some(capabilities) = poster.capabilities() {
        capabilities.require(&config)?;
    }
    let qp_num = poster.qp_num();
    let registration = shared.connections.register(qp_num, |token| {
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
    let (_, state) = match registration {
        Ok(registration) => registration,
        Err(error) => {
            if matches!(error, Error::CapacityExhausted) {
                shared
                    .diagnostic_counters
                    .connection_capacity_exhausted
                    .fetch_add(1, Ordering::Relaxed);
            }
            return Err(error);
        }
    };
    Ok(RdmaConnection {
        shared: Arc::clone(shared),
        state,
    })
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
                    config.max_send_wr_value(),
                ),
                (
                    "maximum receive WRs",
                    self.max_recv_wr as usize,
                    config.max_recv_wr_value(),
                ),
                (
                    "maximum send SGEs",
                    self.max_send_sge as usize,
                    config.max_send_sge_value(),
                ),
                (
                    "maximum receive SGEs",
                    self.max_recv_sge as usize,
                    config.max_recv_sge_value(),
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
        let outcome = CloseOutcome::Failed(Error::InvalidConfig("typed close failure".into()));
        for result in [outcome.clone().into_result(), outcome.into_result()] {
            assert!(
                matches!(result, Err(Error::InvalidConfig(ref message)) if message == "typed close failure")
            );
        }
    }
}
