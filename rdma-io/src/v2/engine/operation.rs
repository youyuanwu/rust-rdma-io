//! Owned low-level operation futures, admission, and exact CQE routing.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::EngineShared;
use super::connection::{ConnectionState, Direction, OperationKind};
use super::diagnostics::CqeReject;
use super::registry::{ConnectionToken, Lookup, OperationToken, PagedRegistry, lock_unpoison};
use crate::v2::error::{Error, Result};
use crate::v2::mr::{Mr, RemoteMr};
use crate::v2::op::Completion;
use crate::v2::qp::BatchPostOutcome;
use crate::wc::{WcOpcode, WorkCompletion};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

/// Future for one engine-owned SEND, RECV, READ, or WRITE.
///
/// The future owns its MR. Dropping it after posting transfers observation to
/// the engine; the MR and admission debt remain registered until the exact CQE
/// is consumed or a later phase establishes another positive safety boundary.
pub struct RdmaOperation {
    state: FutureState,
}

enum FutureState {
    PrePost {
        shared: Arc<EngineShared>,
        connection: Arc<ConnectionState>,
        kind: OperationKind,
        mr: Option<Mr>,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    },
    InFlight {
        shared: Arc<EngineShared>,
        operation: Arc<OperationState>,
    },
    Immediate(Option<(Result<Completion>, Option<Mr>)>),
    Done,
}

impl Unpin for RdmaOperation {}

impl RdmaOperation {
    pub(super) fn new(
        shared: Arc<EngineShared>,
        connection: Arc<ConnectionState>,
        kind: OperationKind,
        mr: Mr,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    ) -> Self {
        Self {
            state: FutureState::PrePost {
                shared,
                connection,
                kind,
                mr: Some(mr),
                remote,
                range,
            },
        }
    }
}

impl Future for RdmaOperation {
    type Output = (Result<Completion>, Option<Mr>);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match &mut self.state {
                FutureState::PrePost { .. } => {
                    let pending = std::mem::replace(&mut self.state, FutureState::Done);
                    let FutureState::PrePost {
                        shared,
                        connection,
                        kind,
                        mut mr,
                        remote,
                        range,
                    } = pending
                    else {
                        return Poll::Ready((Err(Error::DriverShutdown), None));
                    };
                    let Some(mr) = mr.take() else {
                        return Poll::Ready((Err(Error::DriverShutdown), None));
                    };
                    self.state =
                        match start_operation(&shared, &connection, kind, mr, remote, range) {
                            StartResult::InFlight(operation) => {
                                FutureState::InFlight { shared, operation }
                            }
                            StartResult::Immediate(output) => FutureState::Immediate(Some(output)),
                        };
                }
                FutureState::InFlight { operation, .. } => {
                    operation.waker.register(cx.waker());
                    if let Some(output) = operation.take_output() {
                        self.state = FutureState::Done;
                        return Poll::Ready(output);
                    }
                    return Poll::Pending;
                }
                FutureState::Immediate(output) => {
                    let output = output
                        .take()
                        .unwrap_or_else(|| (Err(Error::DriverShutdown), None));
                    self.state = FutureState::Done;
                    return Poll::Ready(output);
                }
                FutureState::Done => return Poll::Ready((Err(Error::DriverShutdown), None)),
            }
        }
    }
}

impl Drop for RdmaOperation {
    fn drop(&mut self) {
        let state = std::mem::replace(&mut self.state, FutureState::Done);
        if let FutureState::InFlight { shared, operation } = state
            && operation.cancel()
        {
            shared
                .diagnostic_counters
                .operations_cancelled
                .fetch_add(1, Ordering::Relaxed);
            shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
            shared.schedule_reclamation(operation.token);
        }
    }
}

pub(super) struct OperationRegistry {
    slots: PagedRegistry<OperationToken, Arc<OperationState>>,
}

impl OperationRegistry {
    pub(super) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            slots: PagedRegistry::new(capacity)?,
        })
    }

    fn allocate(
        &self,
        make: impl FnOnce(OperationToken) -> Arc<OperationState>,
    ) -> Result<OperationToken> {
        self.slots.allocate_with(make)
    }

    pub(super) fn lookup(&self, token: OperationToken) -> Lookup<Arc<OperationState>> {
        self.slots.lookup_cloned(token)
    }

    fn release(&self, token: OperationToken, completed: bool) -> Option<Arc<OperationState>> {
        self.slots.release(token, completed)
    }

    pub(super) fn live(&self) -> usize {
        self.slots.live()
    }

    pub(super) fn retired(&self) -> usize {
        self.slots.retired()
    }

    pub(super) fn free(&self) -> usize {
        self.slots.free()
    }
}

pub(super) struct CqCreditPool {
    capacity: usize,
    used: AtomicUsize,
    retained: AtomicUsize,
}

impl CqCreditPool {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: AtomicUsize::new(0),
            retained: AtomicUsize::new(0),
        }
    }

    fn reserve(&self) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            if used >= self.capacity {
                return false;
            }
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => used = observed,
            }
        }
    }

    fn release(&self) {
        let previous = self.used.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "CQ admission release must have a reservation");
    }

    fn retain(&self) {
        self.retained.fetch_add(1, Ordering::AcqRel);
    }

    fn release_retained(&self) {
        let previous = self.retained.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "retained CQ credit must exist");
    }

    pub(super) fn free(&self) -> usize {
        self.capacity
            .saturating_sub(self.used.load(Ordering::Acquire))
    }

    pub(super) fn retained(&self) -> usize {
        self.retained.load(Ordering::Acquire)
    }
}

pub(super) struct OperationState {
    token: OperationToken,
    connection: Arc<ConnectionState>,
    direction: Direction,
    expected_opcode: WcOpcode,
    mr_len: usize,
    inner: Mutex<OperationInner>,
    waker: AtomicWaker,
    cancelled: AtomicBool,
    quarantined: AtomicBool,
}

struct OperationInner {
    lifecycle: OperationLifecycle,
    mr: Option<Mr>,
    early_completion: Option<WorkCompletion>,
    output: Option<(Result<Completion>, Option<Mr>)>,
    detached: bool,
    reclamation_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationLifecycle {
    #[allow(dead_code, reason = "the public future owns the pre-post phase")]
    PrePost,
    Posting,
    InFlight,
    Completing,
    Cancelled,
    Reclaiming,
    Quarantined,
    Released,
}

impl OperationState {
    fn new(
        token: OperationToken,
        connection: Arc<ConnectionState>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
    ) -> Self {
        Self {
            token,
            connection,
            direction,
            expected_opcode,
            mr_len,
            inner: Mutex::new(OperationInner {
                lifecycle: OperationLifecycle::Posting,
                mr,
                early_completion: None,
                output: None,
                detached: false,
                reclamation_pending: false,
            }),
            waker: AtomicWaker::new(),
            cancelled: AtomicBool::new(false),
            quarantined: AtomicBool::new(false),
        }
    }

    fn commit_accepted(&self) -> Option<WorkCompletion> {
        let mut inner = lock_unpoison(&self.inner);
        self.connection.add_accepted(self.token);
        if inner.detached {
            inner.lifecycle = OperationLifecycle::Cancelled;
        } else {
            inner.lifecycle = OperationLifecycle::InFlight;
        }
        inner.early_completion.take()
    }

    fn record_completion(&self, completion: WorkCompletion) -> CompletionDisposition {
        let mut inner = lock_unpoison(&self.inner);
        match inner.lifecycle {
            OperationLifecycle::Posting | OperationLifecycle::PrePost => {
                if inner.early_completion.replace(completion).is_some() {
                    CompletionDisposition::Duplicate
                } else {
                    CompletionDisposition::Deferred
                }
            }
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming
            | OperationLifecycle::Quarantined => {
                inner.lifecycle = OperationLifecycle::Completing;
                CompletionDisposition::Complete
            }
            OperationLifecycle::Completing | OperationLifecycle::Released => {
                CompletionDisposition::Duplicate
            }
        }
    }

    fn cancel(&self) -> bool {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut inner = lock_unpoison(&self.inner);
        let mut completed_output = None;
        let cancelled = match inner.lifecycle {
            OperationLifecycle::InFlight => {
                inner.lifecycle = OperationLifecycle::Cancelled;
                inner.detached = true;
                inner.reclamation_pending = true;
                true
            }
            OperationLifecycle::Released => {
                inner.detached = true;
                completed_output = inner.output.take();
                false
            }
            OperationLifecycle::Posting | OperationLifecycle::PrePost => {
                inner.detached = true;
                inner.reclamation_pending = true;
                true
            }
            OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming
            | OperationLifecycle::Quarantined
            | OperationLifecycle::Completing => false,
        };
        drop(inner);
        drop(completed_output);
        cancelled
    }

    fn mark_reclaiming(&self) {
        let mut inner = lock_unpoison(&self.inner);
        if inner.lifecycle == OperationLifecycle::Cancelled {
            inner.lifecycle = OperationLifecycle::Reclaiming;
        }
    }

    fn mark_quarantined(&self) -> bool {
        let mut inner = lock_unpoison(&self.inner);
        match inner.lifecycle {
            OperationLifecycle::Cancelled | OperationLifecycle::Reclaiming => {
                inner.lifecycle = OperationLifecycle::Quarantined;
                inner.reclamation_pending = false;
                self.quarantined.store(true, Ordering::Release);
                true
            }
            _ => false,
        }
    }

    fn finish_completion(&self, completion: WorkCompletion) -> FinishState {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        inner.reclamation_pending = false;
        let was_quarantined = self.quarantined.swap(false, Ordering::AcqRel);
        let mr = inner.mr.take();
        let typed = Completion::from(completion);
        let result = typed.result().map(|()| typed);
        let detached_mr = if inner.detached {
            mr
        } else {
            inner.output = Some((result, mr));
            None
        };
        inner.lifecycle = OperationLifecycle::Released;
        drop(inner);
        drop(detached_mr);
        self.waker.wake();
        FinishState {
            was_reclaiming,
            was_quarantined,
        }
    }

    fn take_mr(&self) -> Option<Mr> {
        lock_unpoison(&self.inner).mr.take()
    }

    fn take_output(&self) -> Option<(Result<Completion>, Option<Mr>)> {
        lock_unpoison(&self.inner).output.take()
    }

    fn detach_with_post_error(&self) {
        let mut inner = lock_unpoison(&self.inner);
        inner.detached = true;
        inner.lifecycle = OperationLifecycle::Cancelled;
        inner.reclamation_pending = true;
        self.cancelled.store(true, Ordering::Release);
    }
}

enum CompletionDisposition {
    Deferred,
    Complete,
    Duplicate,
}

struct FinishState {
    was_reclaiming: bool,
    was_quarantined: bool,
}

enum StartResult {
    InFlight(Arc<OperationState>),
    Immediate((Result<Completion>, Option<Mr>)),
}

/// Take-once ownership ledger paired with stable raw batch storage.
#[allow(
    dead_code,
    reason = "message pre-posting consumes this ledger in Phase 7"
)]
pub(super) struct PreparedBatchOwnership<T> {
    entries: Vec<T>,
}

#[allow(
    dead_code,
    reason = "message pre-posting consumes this ledger in Phase 7"
)]
pub(super) enum BatchOwnershipTransfer<T> {
    Accepted(Vec<T>),
    Partial {
        accepted: Vec<T>,
        unaccepted: Vec<T>,
        source: std::io::Error,
    },
    Ambiguous {
        retained: Vec<T>,
        source: std::io::Error,
    },
}

#[allow(
    dead_code,
    reason = "message pre-posting consumes this ledger in Phase 7"
)]
impl<T> PreparedBatchOwnership<T> {
    pub(super) fn new(entries: Vec<T>) -> Result<Self> {
        if entries.is_empty() {
            return Err(Error::InvalidConfig(
                "batch ownership ledger must not be empty".into(),
            ));
        }
        Ok(Self { entries })
    }

    pub(super) fn consume(mut self, outcome: BatchPostOutcome) -> BatchOwnershipTransfer<T> {
        match outcome {
            BatchPostOutcome::AllAccepted => BatchOwnershipTransfer::Accepted(self.entries),
            BatchPostOutcome::PrefixAccepted {
                accepted,
                first_unaccepted,
                source,
            } if accepted == first_unaccepted && accepted <= self.entries.len() => {
                let unaccepted = self.entries.split_off(accepted);
                BatchOwnershipTransfer::Partial {
                    accepted: self.entries,
                    unaccepted,
                    source,
                }
            }
            BatchPostOutcome::PrefixAccepted { source, .. }
            | BatchPostOutcome::Ambiguous { source } => BatchOwnershipTransfer::Ambiguous {
                retained: self.entries,
                source,
            },
        }
    }
}

fn start_operation(
    shared: &Arc<EngineShared>,
    connection: &Arc<ConnectionState>,
    kind: OperationKind,
    mr: Mr,
    remote: Option<RemoteMr>,
    range: Option<(usize, usize)>,
) -> StartResult {
    shared
        .diagnostic_counters
        .operations_offered
        .fetch_add(1, Ordering::Relaxed);

    let validated = match ValidatedOperation::new(kind, &mr, remote, range) {
        Ok(validated) => validated,
        Err(error) => return StartResult::Immediate((Err(error), Some(mr))),
    };
    let direction = kind.direction();
    if let Err(error) = connection.reserve_local(direction) {
        return StartResult::Immediate((Err(error), Some(mr)));
    }

    let expected_opcode = validated.expected_opcode;
    let mr_len = mr.len();
    let mut mr = Some(mr);
    let token = match shared.operations.allocate(|token| {
        Arc::new(OperationState::new(
            token,
            Arc::clone(connection),
            direction,
            expected_opcode,
            mr.take(),
            mr_len,
        ))
    }) {
        Ok(token) => token,
        Err(error) => {
            connection.release_local(direction);
            shared
                .diagnostic_counters
                .operation_capacity_exhausted
                .fetch_add(1, Ordering::Relaxed);
            return StartResult::Immediate((Err(error), mr));
        }
    };
    let state = match shared.operations.lookup(token) {
        Lookup::Occupied(state) => state,
        _ => {
            connection.release_local(direction);
            return StartResult::Immediate((
                Err(Error::InvalidConfig(
                    "new operation registration was not observable".into(),
                )),
                shared
                    .operations
                    .release(token, false)
                    .and_then(|state| state.take_mr()),
            ));
        }
    };

    if !shared.cq_credits.reserve() {
        let state = shared.operations.release(token, false).unwrap_or(state);
        connection.release_local(direction);
        shared
            .diagnostic_counters
            .cq_capacity_exhausted
            .fetch_add(1, Ordering::Relaxed);
        return StartResult::Immediate((Err(Error::CapacityExhausted), state.take_mr()));
    }

    shared
        .diagnostic_counters
        .batch_posts_attempted
        .fetch_add(1, Ordering::Relaxed);
    let outcome = match validated.post(connection, token) {
        Ok(outcome) => outcome,
        Err(error) => {
            let state = shared.operations.release(token, false).unwrap_or(state);
            shared.cq_credits.release();
            connection.release_local(direction);
            return StartResult::Immediate((Err(error), state.take_mr()));
        }
    };
    match outcome {
        BatchPostOutcome::AllAccepted => {
            shared
                .diagnostic_counters
                .operations_accepted
                .fetch_add(1, Ordering::Relaxed);
            shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
            shared
                .diagnostic_counters
                .operations_posted
                .fetch_add(1, Ordering::Relaxed);
            shared
                .diagnostic_counters
                .batch_accepted_prefix
                .fetch_add(1, Ordering::Relaxed);
            let early = state.commit_accepted();
            if let Some(completion) = early {
                shared.finish_operation(Arc::clone(&state), completion);
            }
            StartResult::InFlight(state)
        }
        BatchPostOutcome::PrefixAccepted {
            accepted,
            first_unaccepted,
            source,
        } if accepted == 0 && first_unaccepted == 0 => {
            let early = {
                let mut inner = lock_unpoison(&state.inner);
                inner.early_completion.take()
            };
            if let Some(completion) = early {
                shared
                    .diagnostic_counters
                    .batch_ambiguous_results
                    .fetch_add(1, Ordering::Relaxed);
                shared
                    .diagnostic_counters
                    .operations_accepted
                    .fetch_add(1, Ordering::Relaxed);
                shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
                state.commit_accepted();
                shared.finish_operation(Arc::clone(&state), completion);
                StartResult::InFlight(state)
            } else {
                shared
                    .diagnostic_counters
                    .operations_unaccepted
                    .fetch_add(1, Ordering::Relaxed);
                shared
                    .diagnostic_counters
                    .batch_unaccepted_suffix
                    .fetch_add(1, Ordering::Relaxed);
                let state = shared.operations.release(token, false).unwrap_or(state);
                shared.cq_credits.release();
                connection.release_local(direction);
                StartResult::Immediate((Err(Error::PostFailed(source)), state.take_mr()))
            }
        }
        BatchPostOutcome::PrefixAccepted { source, .. }
        | BatchPostOutcome::Ambiguous { source } => {
            shared
                .diagnostic_counters
                .batch_ambiguous_results
                .fetch_add(1, Ordering::Relaxed);
            shared
                .diagnostic_counters
                .operations_accepted
                .fetch_add(1, Ordering::Relaxed);
            shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
            let early = state.commit_accepted();
            if let Some(completion) = early {
                shared.finish_operation(Arc::clone(&state), completion);
                StartResult::InFlight(state)
            } else {
                state.detach_with_post_error();
                shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
                shared.schedule_reclamation(token);
                StartResult::Immediate((Err(Error::PostFailed(source)), None))
            }
        }
    }
}

struct ValidatedOperation {
    kind: OperationKind,
    sge: Sge,
    remote: Option<RemoteMr>,
    expected_opcode: WcOpcode,
}

impl ValidatedOperation {
    fn new(
        kind: OperationKind,
        mr: &Mr,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    ) -> Result<Self> {
        let (offset, len) = range.unwrap_or((0, mr.len()));
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::InvalidConfig("operation range overflow".into()))?;
        if end > mr.len() {
            return Err(Error::InvalidConfig(format!(
                "operation range {offset}..{end} exceeds MR length {}",
                mr.len()
            )));
        }
        let length = u32::try_from(len)
            .map_err(|_| Error::InvalidConfig("operation length does not fit u32".into()))?;
        let address = mr
            .addr()
            .checked_add(offset as u64)
            .ok_or_else(|| Error::InvalidConfig("local SGE address overflow".into()))?;
        let expected_opcode = match kind {
            OperationKind::Send => WcOpcode::Send,
            OperationKind::Recv => WcOpcode::Recv,
            OperationKind::Write => WcOpcode::RdmaWrite,
            OperationKind::Read => WcOpcode::RdmaRead,
        };
        match kind {
            OperationKind::Write | OperationKind::Read => {
                let remote = remote.ok_or_else(|| {
                    Error::InvalidConfig("RDMA read/write requires a remote MR".into())
                })?;
                if len > remote.len as usize {
                    return Err(Error::InvalidConfig(format!(
                        "operation length {len} exceeds remote MR length {}",
                        remote.len
                    )));
                }
                remote
                    .addr
                    .checked_add(len as u64)
                    .ok_or_else(|| Error::InvalidConfig("remote address range overflow".into()))?;
            }
            OperationKind::Send | OperationKind::Recv if remote.is_some() => {
                return Err(Error::InvalidConfig(
                    "SEND/RECV must not carry a remote MR".into(),
                ));
            }
            OperationKind::Send | OperationKind::Recv => {}
        }
        Ok(Self {
            kind,
            sge: Sge::new(address, length, mr.lkey()),
            remote,
            expected_opcode,
        })
    }

    fn post(self, connection: &ConnectionState, token: OperationToken) -> Result<BatchPostOutcome> {
        match self.kind {
            OperationKind::Recv => {
                let mut batch =
                    PreparedRecvBatch::new(vec![RecvWr::new(token.encode()).sg(self.sge)])
                        .map_err(Error::from)?;
                Ok(connection.poster.post_recv(&mut batch))
            }
            OperationKind::Send | OperationKind::Write | OperationKind::Read => {
                let opcode = match self.kind {
                    OperationKind::Send => WrOpcode::Send,
                    OperationKind::Write => WrOpcode::RdmaWrite,
                    OperationKind::Read => WrOpcode::RdmaRead,
                    OperationKind::Recv => {
                        return Err(Error::InvalidConfig(
                            "RECV cannot be encoded as a SEND work request".into(),
                        ));
                    }
                };
                let mut wr = SendWr::new(token.encode(), opcode)
                    .sg(self.sge)
                    .flags(SendFlags::SIGNALED);
                if let Some(remote) = self.remote {
                    wr = wr.rdma(remote.addr, remote.rkey);
                }
                let mut batch = PreparedSendBatch::new(vec![wr]).map_err(Error::from)?;
                Ok(connection.poster.post_send(&mut batch))
            }
        }
    }
}

impl EngineShared {
    pub(super) fn enqueue_completion(&self, completion: WorkCompletion) -> Option<ConnectionToken> {
        let token = OperationToken::decode(completion.wr_id());
        let operation = match self.operations.lookup(token) {
            Lookup::Occupied(operation) => operation,
            Lookup::Duplicate => {
                self.diagnostic_counters.reject_cqe(CqeReject::Duplicate);
                return None;
            }
            Lookup::Stale | Lookup::Retired => {
                self.diagnostic_counters
                    .reject_cqe(CqeReject::StaleOperation);
                return None;
            }
            Lookup::Unknown => {
                self.diagnostic_counters.reject_cqe(CqeReject::Unknown);
                return None;
            }
        };
        let connection = match self.connections.lookup(operation.connection.token) {
            Lookup::Occupied(connection) => connection,
            _ => {
                self.diagnostic_counters
                    .reject_cqe(CqeReject::StaleConnection);
                return None;
            }
        };
        if completion.qp_num() != operation.connection.qp_num() {
            self.diagnostic_counters.reject_cqe(CqeReject::WrongQpNum);
            return None;
        }
        if self.connections.lookup_qp(completion.qp_num()) != Some(operation.connection.token) {
            self.diagnostic_counters
                .reject_cqe(CqeReject::WrongConnection);
            return None;
        }
        if completion.is_success() && completion.opcode() != operation.expected_opcode {
            self.diagnostic_counters
                .reject_cqe(CqeReject::UnexpectedOpcode);
            return None;
        }
        connection.enqueue_completion(completion);
        Some(connection.token)
    }

    pub(super) fn process_connection_ready(
        &self,
        token: ConnectionToken,
        quantum: usize,
    ) -> (usize, bool) {
        let connection = match self.connections.lookup(token) {
            Lookup::Occupied(connection) => connection,
            _ => return (0, false),
        };
        let mut processed = 0;
        while processed < quantum {
            let Some(completion) = connection.pop_completion() else {
                break;
            };
            self.dispatch_queued_completion(completion);
            processed += 1;
        }
        (processed, connection.has_completion_work())
    }

    fn dispatch_queued_completion(&self, completion: WorkCompletion) {
        let token = OperationToken::decode(completion.wr_id());
        let operation = match self.operations.lookup(token) {
            Lookup::Occupied(operation) => operation,
            Lookup::Duplicate => {
                self.diagnostic_counters.reject_cqe(CqeReject::Duplicate);
                return;
            }
            Lookup::Stale | Lookup::Retired => {
                self.diagnostic_counters
                    .reject_cqe(CqeReject::StaleOperation);
                return;
            }
            Lookup::Unknown => {
                self.diagnostic_counters.reject_cqe(CqeReject::Unknown);
                return;
            }
        };
        match operation.record_completion(completion) {
            CompletionDisposition::Deferred => {}
            CompletionDisposition::Complete => {
                self.finish_operation(operation, completion);
            }
            CompletionDisposition::Duplicate => {
                self.diagnostic_counters.reject_cqe(CqeReject::Duplicate);
            }
        }
    }

    fn finish_operation(&self, operation: Arc<OperationState>, completion: WorkCompletion) {
        if self.operations.release(operation.token, true).is_none() {
            self.diagnostic_counters.reject_cqe(CqeReject::Duplicate);
            return;
        }
        operation.connection.remove_accepted(operation.token);
        operation.connection.release_local(operation.direction);
        self.cq_credits.release();
        self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        let finished = operation.finish_completion(completion);
        if finished.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_mrs.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_bytes
                .fetch_sub(operation.mr_len, Ordering::AcqRel);
        }
        self.diagnostic_counters
            .operations_completed
            .fetch_add(1, Ordering::Relaxed);
        self.diagnostic_counters
            .cqes_routed
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn begin_reclamation(&self, token: OperationToken) {
        if let Lookup::Occupied(operation) = self.operations.lookup(token) {
            operation.mark_reclaiming();
        }
    }

    pub(super) fn handle_reclamation_deadline(&self, token: OperationToken) {
        let Lookup::Occupied(operation) = self.operations.lookup(token) else {
            return;
        };
        if !operation.mark_quarantined() {
            return;
        }
        self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
        self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
        self.quarantined_bytes
            .fetch_add(operation.mr_len, Ordering::AcqRel);
        self.cq_credits.retain();
        self.diagnostic_counters
            .reclamation_deadlines
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn handle_connection_drain_deadline(&self, token: ConnectionToken) {
        let Lookup::Occupied(connection) = self.connections.lookup(token) else {
            return;
        };
        connection.apply_drain_deadline();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::engine::config::EngineConfig;
    use crate::v2::engine::connection::{WorkRequestPoster, install_connection};
    use crate::v2::engine::resources::ResourceSummary;
    use rdma_io_sys::ibverbs::{IBV_WC_RECV, IBV_WC_SEND, IBV_WC_SUCCESS};

    #[test]
    fn credit_pool_never_oversubscribes_or_reuses_retained_debt() {
        let credits = CqCreditPool::new(2);
        assert!(credits.reserve());
        assert!(credits.reserve());
        assert!(!credits.reserve());
        credits.retain();
        assert_eq!(credits.free(), 0);
        assert_eq!(credits.retained(), 1);
        credits.release();
        assert_eq!(credits.free(), 1);
        assert_eq!(credits.retained(), 1);
        credits.release_retained();
        credits.release();
        assert_eq!(credits.free(), 2);
    }

    #[test]
    fn operation_registry_retires_and_distinguishes_duplicates() {
        let registry = OperationRegistry::new(1).unwrap();
        let connection = synthetic_connection();
        let token = registry
            .allocate(|token| {
                Arc::new(OperationState::new(
                    token,
                    connection,
                    Direction::Send,
                    WcOpcode::Send,
                    None,
                    64,
                ))
            })
            .unwrap();
        registry.release(token, true).unwrap();
        assert!(matches!(registry.lookup(token), Lookup::Duplicate));
        let reused = registry
            .allocate(|token| {
                Arc::new(OperationState::new(
                    token,
                    synthetic_connection(),
                    Direction::Send,
                    WcOpcode::Send,
                    None,
                    64,
                ))
            })
            .unwrap();
        assert_eq!(reused.slot, token.slot);
        assert_ne!(reused.generation, token.generation);
        assert!(matches!(registry.lookup(token), Lookup::Duplicate));
        registry.release(reused, false).unwrap();
        assert!(matches!(registry.lookup(reused), Lookup::Stale));
    }

    #[test]
    fn every_operation_lifecycle_state_is_non_wrapping_and_explicit() {
        let states = [
            OperationLifecycle::PrePost,
            OperationLifecycle::Posting,
            OperationLifecycle::InFlight,
            OperationLifecycle::Completing,
            OperationLifecycle::Cancelled,
            OperationLifecycle::Reclaiming,
            OperationLifecycle::Quarantined,
            OperationLifecycle::Released,
        ];
        assert_eq!(states.len(), 8);
    }

    #[test]
    fn exact_routing_rejects_unknown_stale_duplicate_qp_and_opcode_classes() {
        let shared = synthetic_engine(8);
        let first = synthetic_connection_on(&shared, 7);
        let exact = install_accepted(&shared, &first.state, WcOpcode::Send);
        let exact_wc = wc(exact, 7, IBV_WC_SEND);
        assert_eq!(shared.enqueue_completion(exact_wc), Some(first.state.token));
        assert_eq!(
            shared.process_connection_ready(first.state.token, 1),
            (1, false)
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .cqes_routed
                .load(Ordering::Acquire),
            1
        );
        assert!(shared.enqueue_completion(exact_wc).is_none());
        assert_eq!(
            shared
                .diagnostic_counters
                .duplicate_cqes
                .load(Ordering::Acquire),
            1
        );

        let unknown = OperationToken {
            slot: 99,
            generation: 99,
        };
        assert!(
            shared
                .enqueue_completion(wc(unknown, 7, IBV_WC_SEND))
                .is_none()
        );

        let wrong_qp = install_accepted(&shared, &first.state, WcOpcode::Send);
        assert!(
            shared
                .enqueue_completion(wc(wrong_qp, 8, IBV_WC_SEND))
                .is_none()
        );

        let wrong_opcode = install_accepted(&shared, &first.state, WcOpcode::Send);
        assert!(
            shared
                .enqueue_completion(wc(wrong_opcode, 7, IBV_WC_RECV))
                .is_none()
        );

        let stale = shared
            .operations
            .allocate(|token| {
                Arc::new(OperationState::new(
                    token,
                    Arc::clone(&first.state),
                    Direction::Send,
                    WcOpcode::Send,
                    None,
                    1,
                ))
            })
            .unwrap();
        shared.operations.release(stale, false).unwrap();
        assert!(
            shared
                .enqueue_completion(wc(stale, 7, IBV_WC_SEND))
                .is_none()
        );

        let second = synthetic_connection_on(&shared, 8);
        let wrong_connection = install_accepted(&shared, &first.state, WcOpcode::Send);
        shared
            .connections
            .set_qp_mapping_for_test(7, second.state.token);
        assert!(
            shared
                .enqueue_completion(wc(wrong_connection, 7, IBV_WC_SEND))
                .is_none()
        );

        let stale_connection = install_accepted(&shared, &second.state, WcOpcode::Send);
        shared
            .connections
            .release(second.state.token, second.state.qp_num());
        assert!(
            shared
                .enqueue_completion(wc(stale_connection, 8, IBV_WC_SEND))
                .is_none()
        );

        assert_eq!(
            shared
                .diagnostic_counters
                .unknown_cqes
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .stale_operation_cqes
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .wrong_qp_num_cqes
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .unexpected_opcode_cqes
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .wrong_connection_cqes
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .stale_connection_cqes
                .load(Ordering::Acquire),
            1
        );
    }

    #[test]
    fn ready_connection_quantum_bounds_routed_work_without_idle_scans() {
        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 17);
        for _ in 0..3 {
            let token = install_accepted(&shared, &connection.state, WcOpcode::Recv);
            assert_eq!(
                shared.enqueue_completion(wc(token, 17, IBV_WC_RECV)),
                Some(connection.state.token)
            );
        }
        assert_eq!(
            shared.process_connection_ready(connection.state.token, 2),
            (2, true)
        );
        assert_eq!(
            shared.process_connection_ready(connection.state.token, 2),
            (1, false)
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .cqes_routed
                .load(Ordering::Acquire),
            3
        );
    }

    #[test]
    fn batch_ownership_transfers_each_prefix_and_ambiguous_batch_once() {
        for first_unaccepted in 0..=4 {
            let transfer = PreparedBatchOwnership::new(vec![0, 1, 2, 3])
                .unwrap()
                .consume(BatchPostOutcome::PrefixAccepted {
                    accepted: first_unaccepted,
                    first_unaccepted,
                    source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                });
            let BatchOwnershipTransfer::Partial {
                accepted,
                unaccepted,
                ..
            } = transfer
            else {
                panic!("valid bad_wr member must split the ledger")
            };
            assert_eq!(accepted, (0..first_unaccepted).collect::<Vec<_>>());
            assert_eq!(unaccepted, (first_unaccepted..4).collect::<Vec<_>>());
        }

        let transfer = PreparedBatchOwnership::new(vec![1, 2, 3]).unwrap().consume(
            BatchPostOutcome::Ambiguous {
                source: std::io::Error::from_raw_os_error(libc::EIO),
            },
        );
        let BatchOwnershipTransfer::Ambiguous { retained, .. } = transfer else {
            panic!("ambiguous bad_wr must retain the complete batch")
        };
        assert_eq!(retained, vec![1, 2, 3]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_operation_deadline_retains_slot_mr_debt_and_late_routing() {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::Context;
        use std::time::Duration;

        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 27);
        let token = install_accepted(&shared, &connection.state, WcOpcode::Recv);
        let Lookup::Occupied(operation) = shared.operations.lookup(token) else {
            panic!("accepted operation")
        };
        assert!(operation.cancel());
        shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
        shared.schedule_reclamation(token);

        let mut driver = super::super::RdmaEngineDriver::new(Arc::clone(&shared), None);
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());

        assert_eq!(shared.operations.live(), 1);
        assert_eq!(shared.cq_credits.free(), 7);
        assert_eq!(shared.cq_credits.retained(), 1);
        assert_eq!(shared.quarantined_operations.load(Ordering::Acquire), 1);
        assert_eq!(shared.quarantined_mrs.load(Ordering::Acquire), 1);
        assert_eq!(shared.quarantined_bytes.load(Ordering::Acquire), 1);

        let completion = wc(token, 27, IBV_WC_RECV);
        assert_eq!(
            shared.enqueue_completion(completion),
            Some(connection.state.token)
        );
        assert_eq!(
            shared.process_connection_ready(connection.state.token, 1),
            (1, false)
        );
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 8);
        assert_eq!(shared.cq_credits.retained(), 0);
        assert_eq!(shared.quarantined_operations.load(Ordering::Acquire), 0);
        assert_eq!(shared.quarantined_bytes.load(Ordering::Acquire), 0);
    }

    struct NoopPoster(u32);

    impl WorkRequestPoster for NoopPoster {
        fn qp_num(&self) -> u32 {
            self.0
        }
        fn capabilities(&self) -> Option<crate::v2::qp::QpCapabilities> {
            None
        }
        fn post_send(&self, _: &mut PreparedSendBatch) -> BatchPostOutcome {
            BatchPostOutcome::AllAccepted
        }
        fn post_recv(&self, _: &mut PreparedRecvBatch) -> BatchPostOutcome {
            BatchPostOutcome::AllAccepted
        }
        fn to_error(&self) -> Result<()> {
            Ok(())
        }
    }

    fn synthetic_engine(capacity: usize) -> Arc<EngineShared> {
        let mut config = EngineConfig::new("test0".into());
        config.max_live_connections = capacity;
        config.max_inflight_operations = capacity;
        config.cq_capacity = capacity;
        Arc::new(
            EngineShared::new(
                config,
                ResourceSummary {
                    contexts: 0,
                    protection_domains: 0,
                    completion_queues: 0,
                    completion_channels: 0,
                    cm_event_channels: 0,
                },
                None,
                None,
            )
            .unwrap(),
        )
    }

    fn synthetic_connection_on(
        shared: &Arc<EngineShared>,
        qp_num: u32,
    ) -> super::super::connection::RdmaConnection {
        install_connection(
            shared,
            Arc::new(NoopPoster(qp_num)),
            super::super::RdmaConnectionConfig::default()
                .max_send_wr(4)
                .max_recv_wr(4),
            None,
            None,
        )
        .unwrap()
    }

    fn synthetic_connection() -> Arc<ConnectionState> {
        synthetic_connection_on(&synthetic_engine(8), 7).state
    }

    fn install_accepted(
        shared: &Arc<EngineShared>,
        connection: &Arc<ConnectionState>,
        opcode: WcOpcode,
    ) -> OperationToken {
        assert!(
            connection
                .reserve_local(match opcode {
                    WcOpcode::Recv => Direction::Recv,
                    _ => Direction::Send,
                })
                .is_ok()
        );
        assert!(shared.cq_credits.reserve());
        let token = shared
            .operations
            .allocate(|token| {
                Arc::new(OperationState::new(
                    token,
                    Arc::clone(connection),
                    match opcode {
                        WcOpcode::Recv => Direction::Recv,
                        _ => Direction::Send,
                    },
                    opcode,
                    None,
                    1,
                ))
            })
            .unwrap();
        let Lookup::Occupied(operation) = shared.operations.lookup(token) else {
            panic!("installed operation");
        };
        operation.commit_accepted();
        shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
        token
    }

    fn wc(token: OperationToken, qp_num: u32, opcode: u32) -> WorkCompletion {
        let mut completion = WorkCompletion::default();
        completion.inner.wr_id = token.encode();
        completion.inner.qp_num = qp_num;
        completion.inner.opcode = opcode;
        completion.inner.status = IBV_WC_SUCCESS;
        completion
    }
}
