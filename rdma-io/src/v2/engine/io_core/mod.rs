//! Low-level operation/completion state composed by the v2 engine.

mod operation;

use std::collections::{HashSet, VecDeque};
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::Duration;

use super::io::{IoEventSender, IoTerminalEvent, PendingIoEvent};
use super::registry::{
    ConnectionToken, OperationToken, lock_unpoison, read_unpoison, write_unpoison,
};
use super::scheduler::{DeadlineKind, DeadlineRequest};
#[cfg(test)]
use super::{
    CompletionMode, RdmaConnectionConfig, RdmaEngine, RdmaEngineBuilder, RdmaEngineDriver,
    RdmaEngineLifecycle, connection, io,
};
use crate::v2::error::{Error, Result};
use crate::v2::qp::BatchPostOutcome;
use crate::wc::WorkCompletion;
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};
pub(super) use operation::CqeReject;
pub use operation::RdmaOperation;
pub(super) use operation::{
    CqCreditPool, IoCoreEffects, OperationQuarantineEffect, OperationRegistry, QpReclaimCapability,
    post_io_recv_batch, post_io_send,
};
#[cfg(test)]
pub(super) use operation::{
    completion_for_driver_test, install_accepted_operation_for_driver_test,
};

/// Posting-only QP authority supplied by the session layer.
///
/// This boundary deliberately excludes QP error transitions, destruction,
/// disconnect, CM ownership, and retirement.
pub(super) trait IoPostAuthority: Send + Sync {
    fn qp_num(&self) -> u32;
    fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome>;
    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome>;
}

/// Restricted publication surface from the I/O core to the explicit driver.
pub(super) trait IoDriverSignal: Send + Sync {
    fn publish_cq_recheck(&self);
    fn publish_completion_dispatch(&self);
    fn publish_reclamation(&self);
    fn publish_terminal(&self);
    #[cfg(any(test, feature = "test-hooks"))]
    fn pause_operation_before_register(&self);
}

/// Immutable session identity accepted by the operation/completion core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EstablishedIoIdentity {
    pub(super) connection: ConnectionToken,
    pub(super) qp_num: u32,
}

/// Opaque posting and operation-ledger capability for one established session.
///
/// The concrete posting authority contains only a weak reference to the
/// session-owned resource bundle.
pub(super) struct EstablishedIoConnection {
    identity: EstablishedIoIdentity,
    poster: Arc<dyn IoPostAuthority>,
    max_send_wr: usize,
    max_recv_wr: usize,
    posting_open: AtomicBool,
    posting_gate: RwLock<()>,
    local_credits: Mutex<LocalCredits>,
    accepted: Mutex<HashSet<AcceptedWrIdentity>>,
    completions: Mutex<VecDeque<WorkCompletion>>,
    io_events: Mutex<Option<IoEventSender>>,
    completion_published: AtomicBool,
    drain_notify: Arc<tokio::sync::Notify>,
}

/// Atomic operation/drain view consumed by session close policy.
pub(super) struct IoDrainReport {
    pub(super) accepted_tokens: Vec<OperationToken>,
}

/// Atomic connection-quarantine accounting reported to session policy.
pub(super) struct IoQuarantineReport {
    pub(super) outstanding_operations: usize,
    pub(super) cq_debt: usize,
}

impl EstablishedIoConnection {
    pub(super) fn new(
        identity: EstablishedIoIdentity,
        poster: Arc<dyn IoPostAuthority>,
        max_send_wr: usize,
        max_recv_wr: usize,
        drain_notify: Arc<tokio::sync::Notify>,
    ) -> Arc<Self> {
        debug_assert_eq!(identity.qp_num, poster.qp_num());
        Arc::new(Self {
            identity,
            poster,
            max_send_wr,
            max_recv_wr,
            posting_open: AtomicBool::new(true),
            posting_gate: RwLock::new(()),
            local_credits: Mutex::new(LocalCredits::default()),
            accepted: Mutex::new(HashSet::new()),
            completions: Mutex::new(VecDeque::new()),
            io_events: Mutex::new(None),
            completion_published: AtomicBool::new(false),
            drain_notify,
        })
    }

    pub(super) fn identity(&self) -> EstablishedIoIdentity {
        self.identity
    }

    pub(super) fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        self.poster.post_send(batch)
    }

    pub(super) fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        self.poster.post_recv(batch)
    }

    pub(super) fn reserve_local(&self, direction: Direction) -> Result<()> {
        if !self.posting_open.load(Ordering::Acquire) {
            return Err(Error::TransportClosed);
        }
        let mut credits = lock_unpoison(&self.local_credits);
        let (used, maximum) = match direction {
            Direction::Send => (&mut credits.send, self.max_send_wr),
            Direction::Recv => (&mut credits.recv, self.max_recv_wr),
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
            connection: self.identity.connection,
            qp_num: self.identity.qp_num,
            operation: token,
        });
    }

    pub(super) fn remove_accepted(&self, token: OperationToken) -> bool {
        let removed = lock_unpoison(&self.accepted).remove(&AcceptedWrIdentity {
            connection: self.identity.connection,
            qp_num: self.identity.qp_num,
            operation: token,
        });
        if removed {
            self.drain_notify.notify_waiters();
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

    pub(super) fn is_posting_open(&self) -> bool {
        self.posting_open.load(Ordering::Acquire)
    }

    pub(super) fn drain_report(&self) -> IoDrainReport {
        let accepted = lock_unpoison(&self.accepted);
        let accepted_tokens = accepted
            .iter()
            .map(|identity| identity.operation)
            .collect::<Vec<_>>();
        IoDrainReport { accepted_tokens }
    }

    pub(super) fn begin_connection_quarantine(
        &self,
        quarantined: &AtomicBool,
    ) -> Option<IoQuarantineReport> {
        let accepted = lock_unpoison(&self.accepted);
        let outstanding = accepted.len();
        if outstanding == 0 || quarantined.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(IoQuarantineReport {
            outstanding_operations: outstanding,
            cq_debt: outstanding,
        })
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

    pub(super) fn install_io_event_sender(&self, sender: IoEventSender) -> Result<bool> {
        let mut current = lock_unpoison(&self.io_events);
        if current.is_some() {
            return Err(Error::InvalidConfig(
                "connection already has an attached I/O event port".into(),
            ));
        }
        *current = Some(sender);
        let already_terminal = !self.posting_open.load(Ordering::Acquire);
        drop(current);
        Ok(already_terminal)
    }

    pub(super) fn pending_io_event(&self, event: IoTerminalEvent) -> Option<PendingIoEvent> {
        let sender = lock_unpoison(&self.io_events).clone();
        sender.map(|sender| sender.terminal(event))
    }

    pub(super) fn mark_completion_published(&self) -> bool {
        !self.completion_published.swap(true, Ordering::AcqRel)
    }

    pub(super) fn clear_completion_published(&self) {
        self.completion_published.store(false, Ordering::Release);
    }

    pub(super) fn close_posting(&self) {
        let _posting = write_unpoison(&self.posting_gate);
        self.posting_open.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn io_event_lock_available(&self) -> bool {
        self.io_events.try_lock().is_ok()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct AcceptedWrIdentity {
    connection: ConnectionToken,
    qp_num: u32,
    operation: OperationToken,
}

/// State owned by the low-level operation/completion runtime.
pub(super) struct IoCore {
    pub(super) operations: OperationRegistry,
    pub(super) cq_credits: CqCreditPool,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqes: AtomicU64,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqe_reasons: Mutex<Vec<CqeReject>>,
    pub(super) accepted_operations: AtomicUsize,
    pub(super) pending_reclamations: AtomicUsize,
    pub(super) quarantined_operations: AtomicUsize,
    pub(super) quarantined_mrs: AtomicUsize,
    pub(super) quarantined_bytes: AtomicUsize,
    pub(super) published_completion_connections: Mutex<VecDeque<Arc<EstablishedIoConnection>>>,
    admission: Arc<RwLock<()>>,
    admission_error: Mutex<Option<Error>>,
    shutdown_requested: AtomicBool,
    driver_signal: Arc<dyn IoDriverSignal>,
    missing_cqe_deadline: Duration,
    completion_dispatch_budget: usize,
    reclamation_requests: Mutex<VecDeque<DeadlineRequest>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IoCoreDiagnostics {
    pub(super) registered_operations: usize,
    pub(super) accepted_operations: usize,
    pub(super) pending_reclamations: usize,
    pub(super) available_cq_credits: usize,
    pub(super) retained_cq_credits: usize,
    pub(super) quarantined_operations: usize,
    pub(super) quarantined_mrs: usize,
    pub(super) quarantined_bytes: usize,
}

impl IoCore {
    pub(super) fn new(
        max_inflight_operations: usize,
        cq_capacity: usize,
        missing_cqe_deadline: Duration,
        completion_dispatch_budget: usize,
        admission: Arc<RwLock<()>>,
        driver_signal: Arc<dyn IoDriverSignal>,
    ) -> Result<(Arc<Self>, QpReclaimCapability)> {
        let core = Arc::new(Self {
            operations: OperationRegistry::new(max_inflight_operations)?,
            cq_credits: CqCreditPool::new(cq_capacity),
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqes: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqe_reasons: Mutex::new(Vec::new()),
            accepted_operations: AtomicUsize::new(0),
            pending_reclamations: AtomicUsize::new(0),
            quarantined_operations: AtomicUsize::new(0),
            quarantined_mrs: AtomicUsize::new(0),
            quarantined_bytes: AtomicUsize::new(0),
            published_completion_connections: Mutex::new(VecDeque::new()),
            admission,
            admission_error: Mutex::new(None),
            shutdown_requested: AtomicBool::new(false),
            driver_signal,
            missing_cqe_deadline,
            completion_dispatch_budget,
            reclamation_requests: Mutex::new(VecDeque::new()),
        });
        let reclaim = QpReclaimCapability::new(&core);
        Ok((core, reclaim))
    }

    pub(super) fn admission(&self) -> RwLockReadGuard<'_, ()> {
        read_unpoison(&self.admission)
    }

    pub(super) fn admission_error(&self) -> Option<Error> {
        lock_unpoison(&self.admission_error).clone()
    }

    pub(super) fn close_admission(&self, error: Option<Error>) {
        self.shutdown_requested.store(true, Ordering::Release);
        *lock_unpoison(&self.admission_error) = error;
    }

    fn publish_cq_recheck(&self) {
        self.driver_signal.publish_cq_recheck();
    }

    pub(super) fn publish_completion_dispatch(&self) {
        self.driver_signal.publish_completion_dispatch();
    }

    fn publish_reclamation(&self) {
        self.driver_signal.publish_reclamation();
    }

    fn publish_terminal_if_drained(&self, previous: usize) {
        if previous == 1 && self.shutdown_requested.load(Ordering::Acquire) {
            self.driver_signal.publish_terminal();
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn pause_operation_before_register(&self) {
        self.driver_signal.pause_operation_before_register();
    }

    fn schedule_reclamation(&self, token: OperationToken) {
        self.begin_reclamation(token);
        let now = tokio::time::Instant::now();
        let at = now.checked_add(self.missing_cqe_deadline).unwrap_or(now);
        lock_unpoison(&self.reclamation_requests).push_back(DeadlineRequest {
            at,
            kind: DeadlineKind::Reclamation,
            token: token.encode(),
        });
        self.publish_reclamation();
    }

    pub(super) fn take_reclamation_requests(&self, budget: usize) -> Vec<DeadlineRequest> {
        let mut requests = lock_unpoison(&self.reclamation_requests);
        let count = requests.len().min(budget);
        requests.drain(..count).collect()
    }

    pub(super) fn has_reclamation_requests(&self) -> bool {
        !lock_unpoison(&self.reclamation_requests).is_empty()
    }

    pub(super) fn diagnostics(&self) -> IoCoreDiagnostics {
        IoCoreDiagnostics {
            registered_operations: self.operations.live(),
            accepted_operations: self.accepted_operations.load(Ordering::Acquire),
            pending_reclamations: self.pending_reclamations.load(Ordering::Acquire),
            available_cq_credits: self.cq_credits.free(),
            retained_cq_credits: self.cq_credits.retained(),
            quarantined_operations: self.quarantined_operations.load(Ordering::Acquire),
            quarantined_mrs: self.quarantined_mrs.load(Ordering::Acquire),
            quarantined_bytes: self.quarantined_bytes.load(Ordering::Acquire),
        }
    }

    pub(super) fn accepted_count(&self) -> usize {
        self.accepted_operations.load(Ordering::Acquire)
    }

    pub(super) fn publish_connection(&self, connection: &Arc<EstablishedIoConnection>) {
        if connection.mark_completion_published() {
            lock_unpoison(&self.published_completion_connections).push_back(Arc::clone(connection));
        }
        self.publish_completion_dispatch();
    }

    pub(super) fn take_published_connection(&self) -> Option<ConnectionToken> {
        let connection = lock_unpoison(&self.published_completion_connections).pop_front()?;
        connection.clear_completion_published();
        Some(connection.identity().connection)
    }

    pub(super) fn has_published_connections(&self) -> bool {
        !lock_unpoison(&self.published_completion_connections).is_empty()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn rejected_cqe_count(&self) -> u64 {
        self.rejected_cqes.load(Ordering::Acquire)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn rejected_cqe_reasons(&self) -> Vec<CqeReject> {
        lock_unpoison(&self.rejected_cqe_reasons).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPostAuthority;

    impl IoPostAuthority for TestPostAuthority {
        fn qp_num(&self) -> u32 {
            17
        }

        fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }
    }

    #[test]
    fn established_io_state_owns_local_accepted_and_completion_ledgers() {
        let connection = EstablishedIoConnection::new(
            EstablishedIoIdentity {
                connection: ConnectionToken {
                    slot: 3,
                    generation: 5,
                },
                qp_num: 17,
            },
            Arc::new(TestPostAuthority),
            1,
            1,
            Arc::new(tokio::sync::Notify::new()),
        );

        connection.reserve_local(Direction::Send).unwrap();
        assert!(matches!(
            connection.reserve_local(Direction::Send),
            Err(Error::CapacityExhausted)
        ));
        connection.release_local(Direction::Send);
        connection.reserve_local(Direction::Recv).unwrap();

        let operation = OperationToken {
            slot: 7,
            generation: 11,
        };
        connection.add_accepted(operation);
        assert_eq!(connection.accepted_tokens(), vec![operation]);
        assert_eq!(connection.accepted_count(), 1);

        connection.enqueue_completion(WorkCompletion::default());
        assert!(connection.has_completion_work());
        assert!(connection.pop_completion().is_some());
        assert!(!connection.has_completion_work());

        assert!(connection.remove_accepted(operation));
        connection.release_local(Direction::Recv);
        connection.close_posting();
        assert!(matches!(
            connection.begin_posting(),
            Err(Error::TransportClosed)
        ));
        assert!(matches!(
            connection.reserve_local(Direction::Recv),
            Err(Error::TransportClosed)
        ));
    }
}
