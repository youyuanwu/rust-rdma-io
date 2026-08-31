//! Engine-owned low-level connection frontend.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

use tokio::sync::Notify;

use self::qp::QpCapabilitiesExt;
use super::operation::RdmaOperation;
use super::registry::{
    ConnectionToken, OperationToken, lock_unpoison, read_unpoison, write_unpoison,
};
use super::{EngineShared, RdmaConnectionConfig};
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
#[derive(Clone)]
pub struct RdmaConnection {
    pub(super) shared: Arc<EngineShared>,
    pub(super) state: Arc<ConnectionState>,
}

impl RdmaConnection {
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

    /// Stop new posting and wait for this phase's exact accepted set to drain.
    ///
    /// Phase 6 adds the canonical QP-ERR and destruction sequence. This phase
    /// already memoizes the connection-local quarantine result while retaining
    /// every unresolved operation, MR, registration, and CQ credit.
    pub async fn close(&self) -> Result<()> {
        self.state.stop_posting();
        if let Some(outcome) = self.shared.outcome() {
            return outcome.into_result();
        }
        if !self.state.close_started.swap(true, Ordering::AcqRel) {
            self.shared.schedule_connection_drain(self.state.token);
        }
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
            if self.state.accepted_count() == 0 {
                self.state.finish_close_success();
                continue;
            }
            notified.await;
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn transition_to_error_for_test(&self) -> Result<()> {
        self.state.poster.to_error()
    }
}

pub(super) struct ConnectionState {
    pub(super) token: ConnectionToken,
    qp_num: u32,
    config: RdmaConnectionConfig,
    pub(super) poster: Arc<dyn WorkRequestPoster>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    posting_open: AtomicBool,
    posting_gate: RwLock<()>,
    local_credits: Mutex<LocalCredits>,
    accepted: Mutex<HashSet<OperationToken>>,
    completions: Mutex<VecDeque<WorkCompletion>>,
    close_started: AtomicBool,
    close_outcome: Mutex<Option<CloseOutcome>>,
    close_notify: Notify,
    quarantined: AtomicBool,
    error_transition_started: AtomicBool,
}

impl ConnectionState {
    #[allow(
        dead_code,
        reason = "used by Phase 3 test installation and Phase 4 CM paths"
    )]
    pub(super) fn new(
        token: ConnectionToken,
        poster: Arc<dyn WorkRequestPoster>,
        config: RdmaConnectionConfig,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
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
            local_credits: Mutex::new(LocalCredits::default()),
            accepted: Mutex::new(HashSet::new()),
            completions: Mutex::new(VecDeque::new()),
            close_started: AtomicBool::new(false),
            close_outcome: Mutex::new(None),
            close_notify: Notify::new(),
            quarantined: AtomicBool::new(false),
            error_transition_started: AtomicBool::new(false),
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
        lock_unpoison(&self.accepted).insert(token);
    }

    pub(super) fn remove_accepted(&self, token: OperationToken) -> bool {
        let removed = lock_unpoison(&self.accepted).remove(&token);
        if removed {
            self.close_notify.notify_waiters();
            if self.accepted_count() == 0 {
                self.finish_close_success();
            }
        }
        removed
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

    pub(super) fn stop_posting(&self) {
        let _posting = write_unpoison(&self.posting_gate);
        self.posting_open.store(false, Ordering::Release);
    }

    pub(super) fn finish_engine(&self, force_error: bool) {
        self.stop_posting();
        if force_error && !self.error_transition_started.swap(true, Ordering::AcqRel) {
            let _ = self.poster.to_error();
        }
        self.close_notify.notify_waiters();
    }

    pub(super) fn apply_drain_deadline(&self) {
        let accepted = lock_unpoison(&self.accepted);
        let outstanding = accepted.len();
        if outstanding == 0 {
            drop(accepted);
            self.finish_close_success();
            return;
        }
        self.quarantined.store(true, Ordering::Release);
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
    }

    pub(super) fn finish_close_success(&self) {
        if !self.close_started.load(Ordering::Acquire) || self.quarantined.load(Ordering::Acquire) {
            return;
        }
        let mut outcome = lock_unpoison(&self.close_outcome);
        if outcome.is_none() {
            *outcome = Some(CloseOutcome::Success);
        }
        drop(outcome);
        self.close_notify.notify_waiters();
    }

    fn close_outcome(&self) -> Option<CloseOutcome> {
        *lock_unpoison(&self.close_outcome)
    }
}

#[derive(Clone, Copy)]
enum CloseOutcome {
    Success,
    Quarantined { outstanding: usize, cq_debt: usize },
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
        }
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

impl OperationKind {
    pub(super) const fn direction(self) -> Direction {
        match self {
            Self::Recv => Direction::Recv,
            Self::Send | Self::Write | Self::Read => Direction::Send,
        }
    }
}

#[allow(
    dead_code,
    reason = "verbs methods are activated through test hooks and Phase 4 CM paths"
)]
pub(super) trait WorkRequestPoster: Send + Sync {
    fn qp_num(&self) -> u32;
    fn capabilities(&self) -> Option<QpCapabilities>;
    fn post_send(&self, batch: &mut PreparedSendBatch) -> BatchPostOutcome;
    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> BatchPostOutcome;
    fn to_error(&self) -> Result<()>;
}

#[allow(
    dead_code,
    reason = "used by Phase 3 test installation and Phase 4 CM paths"
)]
pub(super) struct VerbsConnectionResources {
    qp: Qp,
    // Declared after `qp`: Rust drops the QP before the CM owner.
    _cm_owner: Box<dyn Send + Sync>,
}

impl VerbsConnectionResources {
    #[allow(
        dead_code,
        reason = "used by Phase 3 test installation and Phase 4 CM paths"
    )]
    pub(super) fn new(qp: Qp, cm_owner: impl Send + Sync + 'static) -> Self {
        Self {
            qp,
            _cm_owner: Box::new(cm_owner),
        }
    }
}

impl WorkRequestPoster for VerbsConnectionResources {
    fn qp_num(&self) -> u32 {
        self.qp.qp_num()
    }

    fn capabilities(&self) -> Option<QpCapabilities> {
        Some(self.qp.capabilities())
    }

    fn post_send(&self, batch: &mut PreparedSendBatch) -> BatchPostOutcome {
        self.qp.post_send_batch(batch)
    }

    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> BatchPostOutcome {
        self.qp.post_recv_batch(batch)
    }

    fn to_error(&self) -> Result<()> {
        self.qp.to_error()
    }
}

#[allow(
    dead_code,
    reason = "used by Phase 3 test installation and Phase 4 CM paths"
)]
pub(super) fn install_connection(
    shared: &Arc<EngineShared>,
    poster: Arc<dyn WorkRequestPoster>,
    config: RdmaConnectionConfig,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
) -> Result<RdmaConnection> {
    config.validate(&shared.config, shared.provider.as_ref())?;
    if let Some(capabilities) = poster.capabilities() {
        capabilities.require(&config)?;
    }
    let _admission = read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        return Err(error);
    }
    let qp_num = poster.qp_num();
    let registration = shared.connections.register(qp_num, |token| {
        Arc::new(ConnectionState::new(
            token, poster, config, local_addr, peer_addr,
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

    #[allow(
        dead_code,
        reason = "used by Phase 3 test installation and Phase 4 CM paths"
    )]
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
}
