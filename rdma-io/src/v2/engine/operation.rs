//! Owned low-level operation futures, admission, and exact CQE routing.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::connection::{ConnectionState, Direction, OperationKind, RdmaConnection};
use super::diagnostics::CqeReject;
use super::registry::{
    ConnectionToken, Lookup, OperationToken, PagedRegistry, lock_unpoison, read_unpoison,
};
use super::{EngineOutcome, EngineShared};
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

struct InternalBatchEntry {
    token: OperationToken,
    state: Arc<OperationState>,
    sge: Sge,
}

type InternalPostInput = (Mr, Option<(usize, usize)>, DetachedOperationCallback);

impl RdmaConnection {
    pub(crate) fn post_detached_recv_batch(
        &self,
        entries: Vec<(Mr, DetachedOperationCallback)>,
    ) -> Result<usize> {
        post_detached_batch(
            &self.shared,
            &self.state,
            OperationKind::Recv,
            entries
                .into_iter()
                .map(|(mr, callback)| (mr, None, callback))
                .collect(),
        )
        .map_err(DetachedPostError::into_error)
    }

    pub(crate) fn post_detached_recv(
        &self,
        mr: Mr,
        callback: DetachedOperationCallback,
    ) -> std::result::Result<(), DetachedPostError> {
        post_detached_batch(
            &self.shared,
            &self.state,
            OperationKind::Recv,
            vec![(mr, None, callback)],
        )
        .map(|_| ())
    }

    pub(crate) fn post_detached_send(
        &self,
        mr: Mr,
        len: usize,
        callback: DetachedOperationCallback,
    ) -> std::result::Result<(), DetachedPostError> {
        post_detached_batch(
            &self.shared,
            &self.state,
            OperationKind::Send,
            vec![(mr, Some((0, len)), callback)],
        )
        .map(|_| ())
    }
}

fn post_detached_batch(
    shared: &Arc<EngineShared>,
    connection: &Arc<ConnectionState>,
    kind: OperationKind,
    entries: Vec<InternalPostInput>,
) -> std::result::Result<usize, DetachedPostError> {
    if entries.is_empty() {
        return Err(DetachedPostError::unaccepted(Error::InvalidConfig(
            "detached operation batch must not be empty".into(),
        )));
    }
    let count = entries.len();
    shared
        .diagnostic_counters
        .operations_offered
        .fetch_add(count as u64, Ordering::Relaxed);
    let admission = read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        complete_unreserved_entries(entries, error.clone());
        return Err(DetachedPostError::unaccepted(error));
    }
    let posting = match connection.begin_posting() {
        Ok(posting) => posting,
        Err(error) => {
            complete_unreserved_entries(entries, error.clone());
            return Err(DetachedPostError::unaccepted(error));
        }
    };
    let direction = kind.direction();
    let expected_opcode = match kind {
        OperationKind::Recv => WcOpcode::Recv,
        OperationKind::Send => WcOpcode::Send,
        OperationKind::Write | OperationKind::Read => {
            let error = Error::InvalidConfig("detached batches support only SEND and RECV".into());
            complete_unreserved_entries(entries, error.clone());
            return Err(DetachedPostError::unaccepted(error));
        }
    };
    let mut reserved = Vec::with_capacity(count);
    let mut entries = entries.into_iter();
    while let Some((mr, range, callback)) = entries.next() {
        let validated = match ValidatedOperation::new(kind, &mr, None, range) {
            Ok(validated) => validated,
            Err(error) => {
                rollback_internal_entries(shared, connection, direction, reserved, error.clone());
                callback(DetachedOperationCompletion::Unaccepted {
                    error: error.clone(),
                    mr,
                });
                complete_unreserved_entries(entries, error.clone());
                return Err(DetachedPostError::unaccepted(error));
            }
        };
        if let Err(error) = connection.reserve_local(direction) {
            rollback_internal_entries(shared, connection, direction, reserved, error.clone());
            callback(DetachedOperationCompletion::Unaccepted {
                error: error.clone(),
                mr,
            });
            complete_unreserved_entries(entries, error.clone());
            return Err(DetachedPostError::unaccepted(error));
        }
        let mr_len = mr.len();
        let mut mr = Some(mr);
        let mut callback = Some(callback);
        let (token, state) = match shared.operations.allocate(|token| {
            Arc::new(OperationState::new_with_callback(
                token,
                Arc::clone(connection),
                direction,
                expected_opcode,
                mr.take(),
                mr_len,
                callback.take(),
            ))
        }) {
            Ok(allocated) => allocated,
            Err(error) => {
                connection.release_local(direction);
                let mr = mr
                    .take()
                    .expect("operation allocation failure retains detached MR");
                let callback = callback
                    .take()
                    .expect("operation allocation failure retains detached callback");
                rollback_internal_entries(shared, connection, direction, reserved, error.clone());
                callback(DetachedOperationCompletion::Unaccepted {
                    error: error.clone(),
                    mr,
                });
                complete_unreserved_entries(entries, error.clone());
                shared
                    .diagnostic_counters
                    .operation_capacity_exhausted
                    .fetch_add(1, Ordering::Relaxed);
                return Err(DetachedPostError::unaccepted(error));
            }
        };
        if !shared.cq_credits.reserve() {
            let released = shared.operations.release(token, false).unwrap_or(state);
            connection.release_local(direction);
            let error = Error::CapacityExhausted;
            let completion = released.take_unaccepted(error.clone());
            rollback_internal_entries(shared, connection, direction, reserved, error.clone());
            invoke_detached_completion(completion);
            complete_unreserved_entries(entries, error.clone());
            shared
                .diagnostic_counters
                .cq_capacity_exhausted
                .fetch_add(1, Ordering::Relaxed);
            return Err(DetachedPostError::unaccepted(error));
        }
        shared
            .diagnostic_counters
            .cq_credits_reserved
            .fetch_add(1, Ordering::Relaxed);
        reserved.push(InternalBatchEntry {
            token,
            state,
            sge: validated.sge,
        });
    }

    let requests = match kind {
        OperationKind::Recv => {
            let requests = reserved
                .iter()
                .map(|entry| RecvWr::new(entry.token.encode()).sg(entry.sge))
                .collect();
            match PreparedRecvBatch::new(requests) {
                Ok(batch) => InternalPreparedBatch::Recv(batch),
                Err(error) => {
                    let error = Error::from(error);
                    rollback_internal_entries(
                        shared,
                        connection,
                        direction,
                        reserved,
                        error.clone(),
                    );
                    return Err(DetachedPostError::unaccepted(error));
                }
            }
        }
        OperationKind::Send => {
            let requests = reserved
                .iter()
                .map(|entry| {
                    SendWr::new(entry.token.encode(), WrOpcode::Send)
                        .sg(entry.sge)
                        .flags(SendFlags::SIGNALED)
                })
                .collect();
            match PreparedSendBatch::new(requests) {
                Ok(batch) => InternalPreparedBatch::Send(batch),
                Err(error) => {
                    let error = Error::from(error);
                    rollback_internal_entries(
                        shared,
                        connection,
                        direction,
                        reserved,
                        error.clone(),
                    );
                    return Err(DetachedPostError::unaccepted(error));
                }
            }
        }
        OperationKind::Write | OperationKind::Read => unreachable!(),
    };
    let ownership =
        PreparedBatchOwnership::new(reserved).expect("non-empty detached batch ownership");
    let mut requests = requests;
    shared
        .diagnostic_counters
        .batch_posts_attempted
        .fetch_add(1, Ordering::Relaxed);
    let outcome = match &mut requests {
        InternalPreparedBatch::Recv(batch) => connection.poster.post_recv(batch),
        InternalPreparedBatch::Send(batch) => connection.poster.post_send(batch),
    };
    let transfer = ownership.consume(outcome);
    match transfer {
        BatchOwnershipTransfer::Accepted(accepted) => {
            BatchWrAccounting::from_outcome(count, &BatchPostOutcome::AllAccepted).record(shared);
            commit_internal_entries(shared, accepted);
            drop(posting);
            drop(admission);
            Ok(count)
        }
        BatchOwnershipTransfer::Partial {
            mut accepted,
            unaccepted,
            source,
        } => {
            let error = Error::PostFailed(clone_io_error(&source));
            if unaccepted
                .iter()
                .any(|entry| entry.state.has_early_completion())
            {
                accepted.extend(unaccepted);
                BatchWrAccounting::ambiguous(count).record(shared);
                commit_internal_entries(shared, accepted);
                drop(posting);
                drop(admission);
                Err(DetachedPostError::retained(Error::PostFailed(source)))
            } else {
                BatchWrAccounting::exact_prefix(count, accepted.len()).record(shared);
                let potentially_accepted = !accepted.is_empty();
                commit_internal_entries(shared, accepted);
                rollback_internal_entries(shared, connection, direction, unaccepted, error);
                drop(posting);
                drop(admission);
                Err(DetachedPostError {
                    error: Error::PostFailed(source),
                    potentially_accepted,
                })
            }
        }
        BatchOwnershipTransfer::Ambiguous { retained, source } => {
            BatchWrAccounting::ambiguous(count).record(shared);
            commit_internal_entries(shared, retained);
            drop(posting);
            drop(admission);
            Err(DetachedPostError::retained(Error::PostFailed(source)))
        }
    }
}

enum InternalPreparedBatch {
    Recv(PreparedRecvBatch),
    Send(PreparedSendBatch),
}

fn commit_internal_entries(shared: &EngineShared, entries: Vec<InternalBatchEntry>) {
    shared
        .accepted_operations
        .fetch_add(entries.len(), Ordering::AcqRel);
    let mut early = Vec::new();
    for entry in entries {
        if let Some(completion) = entry.state.commit_accepted() {
            early.push((entry.state, completion));
        }
    }
    for (state, completion) in early {
        shared.finish_operation(state, completion);
    }
}

fn rollback_internal_entries(
    shared: &EngineShared,
    connection: &ConnectionState,
    direction: Direction,
    entries: Vec<InternalBatchEntry>,
    error: Error,
) {
    for entry in entries {
        let state = shared
            .operations
            .release(entry.token, false)
            .unwrap_or(entry.state);
        shared.cq_credits.release();
        shared
            .diagnostic_counters
            .cq_credits_rolled_back
            .fetch_add(1, Ordering::Relaxed);
        connection.release_local(direction);
        invoke_detached_completion(state.take_unaccepted(error.clone()));
    }
}

fn complete_unreserved_entries(entries: impl IntoIterator<Item = InternalPostInput>, error: Error) {
    for (mr, _, callback) in entries {
        callback(DetachedOperationCompletion::Unaccepted {
            error: error.clone(),
            mr,
        });
    }
}

fn invoke_detached_completion(completion: Option<DetachedCompletion>) {
    if let Some((callback, completion)) = completion {
        callback(completion);
    }
}

fn clone_io_error(error: &std::io::Error) -> std::io::Error {
    match error.raw_os_error() {
        Some(code) => std::io::Error::from_raw_os_error(code),
        None => std::io::Error::new(error.kind(), error.to_string()),
    }
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
            && operation.cancel(&shared)
        {
            shared
                .diagnostic_counters
                .operations_cancelled
                .fetch_add(1, Ordering::Relaxed);
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
    ) -> Result<(OperationToken, Arc<OperationState>)> {
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

    pub(super) fn occupied(&self) -> Vec<Arc<OperationState>> {
        self.slots.occupied_cloned()
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

    pub(super) fn retain(&self) {
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
    pub(super) mr_len: usize,
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
    callback: Option<DetachedOperationCallback>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationLifecycle {
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
        Self::new_with_callback(
            token,
            connection,
            direction,
            expected_opcode,
            mr,
            mr_len,
            None,
        )
    }

    fn new_with_callback(
        token: OperationToken,
        connection: Arc<ConnectionState>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
        callback: Option<DetachedOperationCallback>,
    ) -> Self {
        let detached = callback.is_some();
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
                detached,
                reclamation_pending: false,
                callback,
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
            OperationLifecycle::Posting => {
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

    fn cancel(&self, shared: &EngineShared) -> bool {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut inner = lock_unpoison(&self.inner);
        let mut completed_output = None;
        let cancelled = match inner.lifecycle {
            OperationLifecycle::InFlight => {
                inner.lifecycle = OperationLifecycle::Cancelled;
                inner.detached = true;
                shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
                inner.reclamation_pending = true;
                true
            }
            OperationLifecycle::Released => {
                inner.detached = true;
                completed_output = inner.output.take();
                false
            }
            OperationLifecycle::Posting => {
                inner.detached = true;
                shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
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

    pub(super) fn mark_quarantined(&self) -> QuarantineTransition {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        match inner.lifecycle {
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming => {
                inner.lifecycle = OperationLifecycle::Quarantined;
                inner.reclamation_pending = false;
                QuarantineTransition {
                    newly_quarantined: !self.quarantined.swap(true, Ordering::AcqRel),
                    was_reclaiming,
                }
            }
            _ => QuarantineTransition {
                newly_quarantined: false,
                was_reclaiming: false,
            },
        }
    }

    pub(super) fn fail_observer_for_close(&self, error: Error) -> bool {
        let mut inner = lock_unpoison(&self.inner);
        if !inner.detached
            && inner.output.is_none()
            && matches!(
                inner.lifecycle,
                OperationLifecycle::InFlight
                    | OperationLifecycle::Cancelled
                    | OperationLifecycle::Reclaiming
                    | OperationLifecycle::Quarantined
            )
        {
            inner.detached = true;
            inner.output = Some((Err(error), None));
            return true;
        }
        false
    }

    fn finish_completion(&self, completion: WorkCompletion) -> FinishState {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        inner.reclamation_pending = false;
        let was_quarantined = self.quarantined.swap(false, Ordering::AcqRel);
        let mut mr = inner.mr.take();
        let typed = Completion::from(completion);
        let result = typed.result().map(|()| typed);
        let callback = inner.callback.take().map(|callback| {
            let callback_mr = mr.take();
            (
                callback,
                DetachedOperationCompletion::Completed {
                    result: result.clone(),
                    mr: callback_mr,
                },
            )
        });
        let detached_mr = if callback.is_some() {
            None
        } else if inner.detached || inner.output.is_some() {
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
            callback,
        }
    }

    fn take_mr(&self) -> Option<Mr> {
        lock_unpoison(&self.inner).mr.take()
    }

    fn take_unaccepted(&self, error: Error) -> Option<DetachedCompletion> {
        let mut inner = lock_unpoison(&self.inner);
        inner.lifecycle = OperationLifecycle::Released;
        let callback = inner.callback.take()?;
        let mr = inner
            .mr
            .take()
            .expect("unaccepted detached operation retains its MR");
        Some((
            callback,
            DetachedOperationCompletion::Unaccepted { error, mr },
        ))
    }

    fn take_output(&self) -> Option<(Result<Completion>, Option<Mr>)> {
        lock_unpoison(&self.inner).output.take()
    }

    fn has_early_completion(&self) -> bool {
        lock_unpoison(&self.inner).early_completion.is_some()
    }

    fn detach_with_post_error(&self, shared: &EngineShared) {
        let mut inner = lock_unpoison(&self.inner);
        inner.detached = true;
        inner.lifecycle = OperationLifecycle::Cancelled;
        shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
        inner.reclamation_pending = true;
        self.cancelled.store(true, Ordering::Release);
    }

    pub(super) fn finalize_terminal(&self, outcome: &EngineOutcome) -> TerminalizeState {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        let newly_quarantined = match inner.lifecycle {
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming => {
                inner.lifecycle = OperationLifecycle::Quarantined;
                !self.quarantined.swap(true, Ordering::AcqRel)
            }
            OperationLifecycle::Quarantined => false,
            OperationLifecycle::Posting
            | OperationLifecycle::Completing
            | OperationLifecycle::Released => {
                return TerminalizeState {
                    was_reclaiming: false,
                    newly_quarantined: false,
                    should_wake: false,
                };
            }
        };
        inner.reclamation_pending = false;
        if !inner.detached && inner.output.is_none() {
            let error = outcome
                .clone()
                .into_result()
                .err()
                .unwrap_or(Error::DriverShutdown);
            inner.output = Some((Err(error), None));
        }
        drop(inner);
        TerminalizeState {
            was_reclaiming,
            newly_quarantined,
            should_wake: true,
        }
    }

    pub(super) fn wake(&self) {
        self.waker.wake();
    }

    #[cfg(test)]
    fn lifecycle(&self) -> OperationLifecycle {
        lock_unpoison(&self.inner).lifecycle
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
    callback: Option<DetachedCompletion>,
}

type DetachedCompletion = (DetachedOperationCallback, DetachedOperationCompletion);

pub(crate) enum DetachedOperationCompletion {
    Unaccepted {
        error: Error,
        mr: Mr,
    },
    Completed {
        result: Result<Completion>,
        mr: Option<Mr>,
    },
}

pub(crate) type DetachedOperationCallback =
    Box<dyn FnOnce(DetachedOperationCompletion) + Send + 'static>;

pub(crate) struct DetachedPostError {
    error: Error,
    potentially_accepted: bool,
}

impl DetachedPostError {
    fn unaccepted(error: Error) -> Self {
        Self {
            error,
            potentially_accepted: false,
        }
    }

    fn retained(error: Error) -> Self {
        Self {
            error,
            potentially_accepted: true,
        }
    }

    pub(crate) fn potentially_accepted(&self) -> bool {
        self.potentially_accepted
    }

    pub(crate) fn error(&self) -> &Error {
        &self.error
    }

    fn into_error(self) -> Error {
        self.error
    }
}

pub(super) struct QuarantineTransition {
    pub(super) newly_quarantined: bool,
    pub(super) was_reclaiming: bool,
}

pub(super) struct TerminalizeState {
    pub(super) was_reclaiming: bool,
    pub(super) newly_quarantined: bool,
    pub(super) should_wake: bool,
}

enum StartResult {
    InFlight(Arc<OperationState>),
    Immediate((Result<Completion>, Option<Mr>)),
}

/// Take-once ownership ledger paired with stable raw batch storage.
pub(crate) struct PreparedBatchOwnership<T> {
    entries: Vec<T>,
}

pub(crate) enum BatchOwnershipTransfer<T> {
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

impl<T> PreparedBatchOwnership<T> {
    pub(crate) fn new(entries: Vec<T>) -> Result<Self> {
        if entries.is_empty() {
            return Err(Error::InvalidConfig(
                "batch ownership ledger must not be empty".into(),
            ));
        }
        Ok(Self { entries })
    }

    pub(crate) fn consume(mut self, outcome: BatchPostOutcome) -> BatchOwnershipTransfer<T> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BatchWrAccounting {
    accepted_or_ambiguous: usize,
    accepted_prefix: usize,
    unaccepted_suffix: usize,
    ambiguous: bool,
}

impl BatchWrAccounting {
    fn from_outcome(wr_count: usize, outcome: &BatchPostOutcome) -> Self {
        match outcome {
            BatchPostOutcome::AllAccepted => Self {
                accepted_or_ambiguous: wr_count,
                accepted_prefix: wr_count,
                unaccepted_suffix: 0,
                ambiguous: false,
            },
            BatchPostOutcome::PrefixAccepted {
                accepted,
                first_unaccepted,
                ..
            } if accepted == first_unaccepted && *accepted <= wr_count => {
                Self::exact_prefix(wr_count, *accepted)
            }
            BatchPostOutcome::PrefixAccepted { .. } | BatchPostOutcome::Ambiguous { .. } => {
                Self::ambiguous(wr_count)
            }
        }
    }

    fn ambiguous(wr_count: usize) -> Self {
        Self {
            accepted_or_ambiguous: wr_count,
            accepted_prefix: 0,
            unaccepted_suffix: 0,
            ambiguous: true,
        }
    }

    fn exact_prefix(wr_count: usize, accepted: usize) -> Self {
        Self {
            accepted_or_ambiguous: accepted,
            accepted_prefix: accepted,
            unaccepted_suffix: wr_count - accepted,
            ambiguous: false,
        }
    }

    fn record(self, shared: &EngineShared) {
        let counters = &shared.diagnostic_counters;
        counters
            .operations_accepted
            .fetch_add(self.accepted_or_ambiguous as u64, Ordering::Relaxed);
        counters
            .operations_posted
            .fetch_add(self.accepted_or_ambiguous as u64, Ordering::Relaxed);
        counters
            .operations_unaccepted
            .fetch_add(self.unaccepted_suffix as u64, Ordering::Relaxed);
        counters
            .batch_accepted_prefix
            .fetch_add(self.accepted_prefix as u64, Ordering::Relaxed);
        counters
            .batch_unaccepted_suffix
            .fetch_add(self.unaccepted_suffix as u64, Ordering::Relaxed);
        if self.ambiguous {
            counters
                .batch_ambiguous_results
                .fetch_add(1, Ordering::Relaxed);
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
    let admission = read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        return StartResult::Immediate((Err(error), Some(mr)));
    }
    #[cfg(any(test, feature = "test-hooks"))]
    shared
        .test_driver
        .pause_admission(super::driver::test_api::AdmissionPausePoint::OperationBeforeRegister);
    let _posting = match connection.begin_posting() {
        Ok(posting) => posting,
        Err(error) => return StartResult::Immediate((Err(error), Some(mr))),
    };
    let direction = kind.direction();
    if let Err(error) = connection.reserve_local(direction) {
        return StartResult::Immediate((Err(error), Some(mr)));
    }

    let expected_opcode = validated.expected_opcode;
    let mr_len = mr.len();
    let mut mr = Some(mr);
    let (token, state) = match shared.operations.allocate(|token| {
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
        .cq_credits_reserved
        .fetch_add(1, Ordering::Relaxed);

    shared
        .diagnostic_counters
        .batch_posts_attempted
        .fetch_add(1, Ordering::Relaxed);
    let outcome = match validated.post(connection, token) {
        Ok(outcome) => outcome,
        Err(error) => {
            let state = shared.operations.release(token, false).unwrap_or(state);
            shared.cq_credits.release();
            shared
                .diagnostic_counters
                .cq_credits_rolled_back
                .fetch_add(1, Ordering::Relaxed);
            connection.release_local(direction);
            return StartResult::Immediate((Err(error), state.take_mr()));
        }
    };
    match outcome {
        BatchPostOutcome::AllAccepted => {
            BatchWrAccounting::from_outcome(1, &BatchPostOutcome::AllAccepted).record(shared);
            shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
            let early = state.commit_accepted();
            drop(admission);
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
                BatchWrAccounting::ambiguous(1).record(shared);
                shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
                state.commit_accepted();
                drop(admission);
                shared.finish_operation(Arc::clone(&state), completion);
                StartResult::InFlight(state)
            } else {
                BatchWrAccounting::exact_prefix(1, accepted).record(shared);
                let state = shared.operations.release(token, false).unwrap_or(state);
                shared.cq_credits.release();
                shared
                    .diagnostic_counters
                    .cq_credits_rolled_back
                    .fetch_add(1, Ordering::Relaxed);
                connection.release_local(direction);
                StartResult::Immediate((Err(Error::PostFailed(source)), state.take_mr()))
            }
        }
        BatchPostOutcome::PrefixAccepted { source, .. }
        | BatchPostOutcome::Ambiguous { source } => {
            BatchWrAccounting::ambiguous(1).record(shared);
            shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
            let early = state.commit_accepted();
            drop(admission);
            if let Some(completion) = early {
                shared.finish_operation(Arc::clone(&state), completion);
                StartResult::InFlight(state)
            } else {
                state.detach_with_post_error(shared);
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
        let _admission = read_unpoison(&self.admission);
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
            if let Some(completion) = connection.pop_completion() {
                // Completion routing must remain serialized with terminal
                // publication, but protocol callbacks acquire admission for
                // each concrete post/close themselves. Holding this read lock
                // across ready work would re-enter the RwLock and deadlock
                // once a shutdown writer is queued.
                let _admission = read_unpoison(&self.admission);
                self.dispatch_queued_completion(completion);
                processed += 1;
                continue;
            }
            let protocol = connection.process_ready_work(quantum - processed);
            if protocol == 0 {
                break;
            }
            processed += protocol;
        }
        (
            processed,
            connection.has_completion_work() || connection.has_attached_work(),
        )
    }

    pub(super) fn handle_message_hello_deadline(&self, token: ConnectionToken) {
        let Lookup::Occupied(connection) = self.connections.lookup(token) else {
            return;
        };
        connection.handle_message_deadline();
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
        let removed = operation.connection.remove_accepted(operation.token);
        operation.connection.release_local(operation.direction);
        self.cq_credits.release();
        self.diagnostic_counters
            .cq_credits_released
            .fetch_add(1, Ordering::Relaxed);
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        if previous == 1 && self.shutdown_requested.load(Ordering::Acquire) {
            self.work_signal.publish(super::driver::TERMINAL_WORK);
        }
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
        invoke_detached_completion(finished.callback);
        if removed
            && operation.connection.close_started()
            && operation.connection.accepted_count() == 0
        {
            self.recover_connection_quarantine(&operation.connection);
            self.record_connection_drained(&operation.connection);
            self.schedule_connection_retirement(&operation.connection);
        }
    }

    pub(super) fn begin_reclamation(&self, token: OperationToken) {
        if let Lookup::Occupied(operation) = self.operations.lookup(token) {
            operation.mark_reclaiming();
        }
    }

    pub(super) fn handle_reclamation_deadline(&self, token: OperationToken) {
        if self.quarantine_operation(token) {
            self.diagnostic_counters
                .reclamation_deadlines
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn quarantine_operation(&self, token: OperationToken) -> bool {
        let Lookup::Occupied(operation) = self.operations.lookup(token) else {
            return false;
        };
        let transition = operation.mark_quarantined();
        if !transition.newly_quarantined {
            return false;
        }
        if transition.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
        self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
        self.quarantined_bytes
            .fetch_add(operation.mr_len, Ordering::AcqRel);
        self.cq_credits.retain();
        self.diagnostic_counters
            .cq_credits_retained
            .fetch_add(1, Ordering::Relaxed);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::destruction::{DestructionKind, DestructionRecorder};
    use crate::v2::AccessIntent;
    use crate::v2::engine::config::EngineConfig;
    use crate::v2::engine::connection::{WorkRequestPoster, install_connection};
    use crate::v2::engine::resources::ResourceSummary;
    use crate::v2::engine::{ConnectionReadyWork, EngineFailure};
    use crate::v2::message_transport::TestEngineHelloDeadlineState;
    use rdma_io_sys::ibverbs::{IBV_WC_RECV, IBV_WC_SEND, IBV_WC_SUCCESS};
    use std::sync::Barrier;
    use std::sync::Weak;
    use std::sync::mpsc;
    use std::time::Duration;

    struct ShutdownPostingReadyWork {
        shared: Weak<EngineShared>,
        connection: Weak<ConnectionState>,
        mr: Mutex<Option<Mr>>,
        start_shutdown: Mutex<Option<mpsc::Sender<()>>>,
        shutdown_complete: Mutex<mpsc::Receiver<()>>,
        result: Mutex<Option<Result<()>>>,
    }

    impl ConnectionReadyWork for ShutdownPostingReadyWork {
        fn process(&self, _budget: usize) -> usize {
            if let Some(start) = lock_unpoison(&self.start_shutdown).take() {
                start.send(()).unwrap();
            }
            lock_unpoison(&self.shutdown_complete).recv().unwrap();
            let shared = self.shared.upgrade().expect("engine shared state");
            let state = self.connection.upgrade().expect("connection state");
            let connection = RdmaConnection::from_state(shared, state);
            let mr = lock_unpoison(&self.mr).take().expect("test MR");
            let result = connection
                .post_detached_send(mr, 1, Box::new(|_completion| {}))
                .map_err(DetachedPostError::into_error);
            *lock_unpoison(&self.result) = Some(result);
            1
        }

        fn has_work(&self) -> bool {
            false
        }

        fn deadline_expired(&self) {}

        fn disconnected(&self) {}

        fn terminalize(&self, _error: Error) {}
    }

    struct ClosingReadyWork {
        shared: Weak<EngineShared>,
        connection: Weak<ConnectionState>,
        race: Arc<Barrier>,
    }

    impl ConnectionReadyWork for ClosingReadyWork {
        fn process(&self, _budget: usize) -> usize {
            self.race.wait();
            let shared = self.shared.upgrade().expect("engine shared state");
            let connection = self.connection.upgrade().expect("connection state");
            shared.begin_connection_close(&connection);
            1
        }

        fn has_work(&self) -> bool {
            false
        }

        fn deadline_expired(&self) {}

        fn disconnected(&self) {}

        fn terminalize(&self, _error: Error) {}
    }

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
        let (token, _) = registry
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
        let (reused, _) = registry
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
    fn operation_lifecycle_transitions_follow_real_post_cancel_and_completion_paths() {
        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 6);
        connection.state.reserve_local(Direction::Send).unwrap();
        assert!(shared.cq_credits.reserve());
        let (token, operation) = shared
            .operations
            .allocate(|token| {
                Arc::new(OperationState::new(
                    token,
                    Arc::clone(&connection.state),
                    Direction::Send,
                    WcOpcode::Send,
                    None,
                    1,
                ))
            })
            .unwrap();
        assert_eq!(operation.lifecycle(), OperationLifecycle::Posting);

        let completion = wc(token, 6, IBV_WC_SEND);
        assert!(matches!(
            operation.record_completion(completion),
            CompletionDisposition::Deferred
        ));
        assert_eq!(operation.lifecycle(), OperationLifecycle::Posting);
        let early = operation.commit_accepted().expect("early completion");
        shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
        assert_eq!(operation.lifecycle(), OperationLifecycle::InFlight);
        shared.finish_operation(Arc::clone(&operation), early);
        assert_eq!(operation.lifecycle(), OperationLifecycle::Released);

        let token = install_accepted(&shared, &connection.state, WcOpcode::Send);
        let Lookup::Occupied(operation) = shared.operations.lookup(token) else {
            panic!("accepted operation")
        };
        assert_eq!(operation.lifecycle(), OperationLifecycle::InFlight);
        assert!(operation.cancel(&shared));
        assert_eq!(operation.lifecycle(), OperationLifecycle::Cancelled);
        shared.begin_reclamation(token);
        assert_eq!(operation.lifecycle(), OperationLifecycle::Reclaiming);
        shared.handle_reclamation_deadline(token);
        assert_eq!(operation.lifecycle(), OperationLifecycle::Quarantined);
        let completion = wc(token, 6, IBV_WC_SEND);
        assert!(matches!(
            operation.record_completion(completion),
            CompletionDisposition::Complete
        ));
        assert_eq!(operation.lifecycle(), OperationLifecycle::Completing);
        shared.finish_operation(Arc::clone(&operation), completion);
        assert_eq!(operation.lifecycle(), OperationLifecycle::Released);
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

        let (stale, _) = shared
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
    fn duplicate_connection_installation_releases_new_slot_without_replacing_qp_index() {
        let shared = synthetic_engine(8);
        let first = synthetic_connection_on(&shared, 9);
        let duplicate = install_connection(
            &shared,
            Arc::new(NoopPoster(9)),
            super::super::RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
        );
        assert!(matches!(duplicate, Err(Error::InvalidConfig(_))));
        assert_eq!(shared.connections.live(), 1);
        assert_eq!(shared.connections.free(), 7);
        assert_eq!(
            shared.connections.lookup_qp(9),
            Some(first.state.token),
            "the original exact qp_num mapping must remain installed"
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
    fn ready_work_post_rechecks_terminal_state_after_shutdown_writer() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 18, ScriptedPost::Accepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let (start_shutdown, shutdown_started) = mpsc::channel();
        let (shutdown_finished, shutdown_complete) = mpsc::channel();
        let work = Arc::new(ShutdownPostingReadyWork {
            shared: Arc::downgrade(&shared),
            connection: Arc::downgrade(&connection.state),
            mr: Mutex::new(Some(mr)),
            start_shutdown: Mutex::new(Some(start_shutdown)),
            shutdown_complete: Mutex::new(shutdown_complete),
            result: Mutex::new(None),
        });
        connection
            .state
            .attach_ready_work(Arc::clone(&work) as Arc<dyn ConnectionReadyWork>)
            .unwrap();

        let shutdown_shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            shutdown_started.recv().unwrap();
            assert!(shutdown_shared.mark_shutdown_requested());
            shutdown_finished.send(()).unwrap();
        });
        let process_shared = Arc::clone(&shared);
        let token = connection.state.token;
        let (process_finished, process_complete) = mpsc::channel();
        std::thread::spawn(move || {
            let result = process_shared.process_connection_ready(token, 1);
            process_finished.send(result).unwrap();
        });

        assert_eq!(
            process_complete
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            (1, false),
            "ready work must not retain/re-enter admission while shutdown waits"
        );
        assert!(matches!(
            lock_unpoison(&work.result).take(),
            Some(Err(Error::DriverShutdown))
        ));
        assert_eq!(poster.calls(), 0);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);

        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn ready_work_and_lifecycle_failure_have_no_abba_lock_cycle() {
        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 19);
        let race = Arc::new(Barrier::new(2));
        connection
            .state
            .attach_ready_work(Arc::new(ClosingReadyWork {
                shared: Arc::downgrade(&shared),
                connection: Arc::downgrade(&connection.state),
                race: Arc::clone(&race),
            }))
            .unwrap();

        let process_shared = Arc::clone(&shared);
        let token = connection.state.token;
        let (process_finished, process_complete) = mpsc::channel();
        std::thread::spawn(move || {
            let result = process_shared.process_connection_ready(token, 1);
            process_finished.send(result).unwrap();
        });

        let failure_connection = Arc::clone(&connection.state);
        let (failure_finished, failure_complete) = mpsc::channel();
        std::thread::spawn(move || {
            let lifecycle = failure_connection.lock_lifecycle();
            race.wait();
            failure_connection.mark_cm_failure(Error::DriverShutdown);
            drop(lifecycle);
            failure_finished.send(()).unwrap();
        });

        failure_complete
            .recv_timeout(Duration::from_secs(5))
            .expect("lifecycle failure must not wait on a callback-held ready_work mutex");
        assert_eq!(
            process_complete
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            (1, false)
        );
        assert!(connection.state.close_started());
    }

    #[tokio::test(start_paused = true)]
    async fn message_hello_deadlines_use_real_queue_timer_and_contextual_wake() {
        use crate::v2::engine::config::{
            DEFAULT_MESSAGE_HELLO_DEADLINE, MAX_MESSAGE_HELLO_DEADLINE, MIN_MESSAGE_HELLO_DEADLINE,
        };

        for (index, deadline) in [
            MIN_MESSAGE_HELLO_DEADLINE,
            DEFAULT_MESSAGE_HELLO_DEADLINE,
            MAX_MESSAGE_HELLO_DEADLINE,
        ]
        .into_iter()
        .enumerate()
        {
            let shared = synthetic_engine_with_hello_deadline(8, deadline);
            let connection = synthetic_connection_on(&shared, 20 + index as u32);
            let probe = TestEngineHelloDeadlineState::new();
            connection.attach_ready_work(probe.ready_work()).unwrap();
            let waiter_probe = probe.clone();
            let waiter = tokio::spawn(async move { waiter_probe.ready().await });
            tokio::task::yield_now().await;

            let mut driver = super::super::RdmaEngineDriver::new(Arc::clone(&shared), None);
            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());

            tokio::time::advance(deadline - Duration::from_nanos(1)).await;
            assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
            assert!(!waiter.is_finished());

            tokio::time::advance(Duration::from_nanos(1)).await;
            assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
            let error = waiter.await.unwrap().unwrap_err();
            assert!(matches!(
                error,
                Error::ProtocolViolation(message) if message == "HELLO handshake timeout"
            ));

            drop(driver);
            drop(connection);
        }
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

    #[test]
    fn batch_counters_count_wrs_and_exact_partial_prefixes() {
        let shared = synthetic_engine(8);
        let partial = BatchPostOutcome::PrefixAccepted {
            accepted: 2,
            first_unaccepted: 2,
            source: std::io::Error::from_raw_os_error(libc::ENOMEM),
        };
        BatchWrAccounting::from_outcome(5, &partial).record(&shared);
        assert_eq!(
            shared
                .diagnostic_counters
                .operations_accepted
                .load(Ordering::Acquire),
            2
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .operations_posted
                .load(Ordering::Acquire),
            2
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .operations_unaccepted
                .load(Ordering::Acquire),
            3
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .batch_accepted_prefix
                .load(Ordering::Acquire),
            2
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .batch_unaccepted_suffix
                .load(Ordering::Acquire),
            3
        );

        let ambiguous = BatchPostOutcome::Ambiguous {
            source: std::io::Error::from_raw_os_error(libc::EIO),
        };
        BatchWrAccounting::from_outcome(4, &ambiguous).record(&shared);
        assert_eq!(
            shared
                .diagnostic_counters
                .operations_accepted
                .load(Ordering::Acquire),
            6
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .operations_posted
                .load(Ordering::Acquire),
            6
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .batch_accepted_prefix
                .load(Ordering::Acquire),
            2
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .batch_ambiguous_results
                .load(Ordering::Acquire),
            1
        );
    }

    #[test]
    fn validation_failure_is_a_zero_call_full_rollback() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 41, ScriptedPost::Accepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 2, 2);
        let mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let mut operation = connection.send(mr, Some((63, 2)));
        let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
            panic!("invalid range must fail synchronously")
        };
        assert!(matches!(result, Err(Error::InvalidConfig(_))));
        assert!(returned.is_some());
        assert_eq!(poster.calls(), 0);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        assert_eq!(connection.state.accepted_count(), 0);

        let mut operation = connection.send(returned.unwrap(), None);
        assert!(poll_once(&mut operation).is_pending());
        let token = poster.tokens()[0];
        complete(&shared, &connection.state, token, IBV_WC_SEND);
        drop(operation);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn cancellation_before_first_poll_posts_nothing_and_releases_unregistered_mr() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 50, ScriptedPost::Accepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let recorder = DestructionRecorder::arm(4);
        let operation = connection.send(
            shared.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            None,
        );
        drop(operation);
        assert_eq!(poster.calls(), 0);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        assert_eq!(
            recorder
                .snapshot()
                .iter()
                .filter(|event| event.kind == DestructionKind::MemoryRegion)
                .count(),
            1
        );
        drop(connection);
        drop(driver);
        drop(engine);
        drop(recorder);
    }

    #[test]
    fn local_direction_exhaustion_posts_nothing_and_preserves_global_capacity() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 42, ScriptedPost::Accepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let first_mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let second_mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let mut first = connection.send(first_mr, None);
        assert!(poll_once(&mut first).is_pending());
        let mut second = connection.send(second_mr, None);
        let Poll::Ready((result, returned)) = poll_once(&mut second) else {
            panic!("local exhaustion must be synchronous")
        };
        assert!(matches!(result, Err(Error::CapacityExhausted)));
        assert!(returned.is_some());
        assert_eq!(poster.calls(), 1);
        assert_eq!(shared.operations.live(), 1);
        assert_eq!(shared.cq_credits.free(), 3);
        assert_eq!(connection.state.accepted_count(), 1);

        complete(&shared, &connection.state, poster.tokens()[0], IBV_WC_SEND);
        drop(first);
        drop(returned);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn operation_global_exhaustion_precedes_and_preserves_the_cq_invariant() {
        let Some((engine, driver)) = production_engine(3, 2, 2) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let first_poster = Arc::new(ScriptedPoster::new(&shared, 43, ScriptedPost::Accepted));
        let second_poster = Arc::new(ScriptedPoster::new(&shared, 44, ScriptedPost::Accepted));
        let first = scripted_connection(&shared, Arc::clone(&first_poster), 1, 1);
        let second = scripted_connection(&shared, Arc::clone(&second_poster), 1, 1);

        let mut send = first.send(
            shared.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            None,
        );
        let mut recv = first.recv(
            shared.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            None,
        );
        assert!(poll_once(&mut send).is_pending());
        assert!(poll_once(&mut recv).is_pending());
        let mut rejected = second.send(
            shared.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            None,
        );
        let Poll::Ready((result, returned)) = poll_once(&mut rejected) else {
            panic!("global operation exhaustion must be synchronous")
        };
        assert!(matches!(result, Err(Error::CapacityExhausted)));
        assert!(returned.is_some());
        assert_eq!(second_poster.calls(), 0);
        assert_eq!(shared.operations.live(), 2);
        assert_eq!(shared.cq_credits.free(), 0);
        assert_eq!(
            shared
                .diagnostic_counters
                .operation_capacity_exhausted
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            shared
                .diagnostic_counters
                .cq_capacity_exhausted
                .load(Ordering::Acquire),
            0,
            "validated max_inflight_operations <= cq_capacity makes CQ exhaustion unreachable"
        );
        second
            .state
            .reserve_local(Direction::Send)
            .expect("global rejection must restore the connection-local credit");
        second.state.release_local(Direction::Send);

        let tokens = first_poster.tokens();
        complete(&shared, &first.state, tokens[0], IBV_WC_SEND);
        complete(&shared, &first.state, tokens[1], IBV_WC_RECV);
        drop(send);
        drop(recv);
        drop(returned);
        drop(first);
        drop(second);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn wholly_unaccepted_post_restores_mr_slot_local_and_cq_reservations() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 45, ScriptedPost::Unaccepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let mut operation = connection.send(
            shared.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            None,
        );
        let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
            panic!("provider-proven rejection must return immediately")
        };
        assert!(matches!(result, Err(Error::PostFailed(_))));
        assert!(returned.is_some());
        assert_eq!(poster.calls(), 1);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        assert_eq!(connection.state.accepted_count(), 0);
        connection
            .state
            .reserve_local(Direction::Send)
            .expect("proven-unaccepted rollback must restore local credit");
        connection.state.release_local(Direction::Send);
        let diagnostics = engine.diagnostics();
        assert_eq!(diagnostics.operations_accepted, 0);
        assert_eq!(diagnostics.operations_posted, 0);
        assert_eq!(diagnostics.operations_unaccepted, 1);
        assert_eq!(diagnostics.batch_accepted_prefix, 0);
        assert_eq!(diagnostics.batch_unaccepted_suffix, 1);
        assert_eq!(diagnostics.batch_ambiguous_results, 0);

        drop(returned);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn empty_and_zero_accepted_detached_batches_reject_and_roll_back_every_reservation() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 51, ScriptedPost::Unaccepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 2);

        assert!(matches!(
            connection.post_detached_recv_batch(Vec::new()),
            Err(Error::InvalidConfig(_))
        ));
        assert_eq!(poster.calls(), 0);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);

        let recorder = DestructionRecorder::arm(8);
        let callback_calls = Arc::new(AtomicUsize::new(0));
        let mut entries = Vec::new();
        for _ in 0..2 {
            let mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
            let callback_calls = Arc::clone(&callback_calls);
            entries.push((
                mr,
                Box::new(move |_completion| {
                    callback_calls.fetch_add(1, Ordering::AcqRel);
                }) as DetachedOperationCallback,
            ));
        }
        assert!(matches!(
            connection.post_detached_recv_batch(entries),
            Err(Error::PostFailed(_))
        ));
        assert_eq!(callback_calls.load(Ordering::Acquire), 2);
        assert_eq!(poster.calls(), 1);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        assert_eq!(connection.state.accepted_count(), 0);
        connection.state.reserve_local(Direction::Recv).unwrap();
        connection.state.reserve_local(Direction::Recv).unwrap();
        connection.state.release_local(Direction::Recv);
        connection.state.release_local(Direction::Recv);
        let diagnostics = engine.diagnostics();
        assert_eq!(diagnostics.operations_offered, 2);
        assert_eq!(diagnostics.operations_accepted, 0);
        assert_eq!(diagnostics.operations_unaccepted, 2);
        assert_eq!(diagnostics.batch_unaccepted_suffix, 2);
        assert_eq!(
            recorder
                .snapshot()
                .iter()
                .filter(|event| event.kind == DestructionKind::MemoryRegion)
                .count(),
            2
        );

        drop(connection);
        drop(driver);
        drop(engine);
        drop(recorder);
    }

    #[test]
    fn ambiguous_acceptance_retains_mr_identity_slot_and_cq_until_exact_dispatch() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 46, ScriptedPost::Ambiguous));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let recorder = DestructionRecorder::arm(8);
        let mut operation = connection.send(mr, None);
        let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
            panic!("ambiguous post reports its contextual error immediately")
        };
        assert!(matches!(result, Err(Error::PostFailed(_))));
        assert!(returned.is_none());
        assert_eq!(shared.operations.live(), 1);
        assert_eq!(shared.cq_credits.free(), 3);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 1);
        assert_eq!(shared.pending_reclamations.load(Ordering::Acquire), 1);
        assert_eq!(connection.state.accepted_count(), 1);
        assert!(recorder.snapshot().is_empty());
        let diagnostics = engine.diagnostics();
        assert_eq!(diagnostics.operations_accepted, 1);
        assert_eq!(diagnostics.operations_posted, 1);
        assert_eq!(diagnostics.operations_unaccepted, 0);
        assert_eq!(diagnostics.batch_accepted_prefix, 0);
        assert_eq!(diagnostics.batch_unaccepted_suffix, 0);
        assert_eq!(diagnostics.batch_ambiguous_results, 1);

        complete(&shared, &connection.state, poster.tokens()[0], IBV_WC_SEND);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        assert_eq!(shared.pending_reclamations.load(Ordering::Acquire), 0);
        assert_eq!(
            recorder
                .snapshot()
                .iter()
                .filter(|event| event.kind == DestructionKind::MemoryRegion)
                .count(),
            1
        );
        drop(operation);
        drop(connection);
        drop(driver);
        drop(engine);
        drop(recorder);
    }

    #[test]
    fn completion_dispatched_during_post_commits_and_releases_exactly_once() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(
            &shared,
            47,
            ScriptedPost::DispatchDuringPost,
        ));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let mut operation = connection.send(
            shared.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            None,
        );
        let Poll::Ready((result, returned)) = poll_once(&mut operation) else {
            panic!("the early exact CQE must be delivered by the first poll")
        };
        result.unwrap();
        assert!(returned.is_some());
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        assert_eq!(engine.diagnostics().operations_completed, 1);
        drop(returned);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn cancellation_and_dispatch_race_releases_each_mr_and_reservation_once() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 48, ScriptedPost::Accepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let recorder = DestructionRecorder::arm(64);

        for iteration in 0..32 {
            let mut operation = connection.send(
                shared.register_memory(64, AccessIntent::LocalOnly).unwrap(),
                None,
            );
            assert!(poll_once(&mut operation).is_pending());
            let token = poster.tokens()[iteration];
            let barrier = Arc::new(Barrier::new(3));
            std::thread::scope(|scope| {
                let drop_barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    drop_barrier.wait();
                    drop(operation);
                });
                let dispatch_barrier = Arc::clone(&barrier);
                let shared = Arc::clone(&shared);
                let connection = Arc::clone(&connection.state);
                scope.spawn(move || {
                    dispatch_barrier.wait();
                    complete(&shared, &connection, token, IBV_WC_SEND);
                });
                barrier.wait();
            });
            assert_eq!(shared.operations.live(), 0);
            assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 0);
            assert_eq!(shared.pending_reclamations.load(Ordering::Acquire), 0);
            assert_eq!(shared.cq_credits.free(), 4);
        }
        assert_eq!(engine.diagnostics().operations_completed, 32);
        assert_eq!(
            recorder
                .snapshot()
                .iter()
                .filter(|event| event.kind == DestructionKind::MemoryRegion)
                .count(),
            32
        );
        drop(connection);
        drop(driver);
        drop(engine);
        drop(recorder);
    }

    #[tokio::test]
    async fn driver_drop_wakes_all_waiters_and_retains_accepted_mrs_fail_closed() {
        use futures_util::task::{ArcWake, waker};

        struct WakeCounter(AtomicUsize);
        impl ArcWake for WakeCounter {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                arc_self.0.fetch_add(1, Ordering::AcqRel);
            }
        }

        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 49, ScriptedPost::Accepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let send_mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let recv_mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let rejected_mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
        let recorder = DestructionRecorder::arm(16);
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = waker(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);

        let mut send = Box::pin(connection.send(send_mr, None));
        let mut recv = Box::pin(connection.recv(recv_mr, None));
        assert!(send.as_mut().poll(&mut cx).is_pending());
        assert!(recv.as_mut().poll(&mut cx).is_pending());
        let mut shutdown = Box::pin(engine.shutdown());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());
        let mut rejected = connection.send(rejected_mr, None);
        let Poll::Ready((result, returned)) = poll_once(&mut rejected) else {
            panic!("shutdown admission barrier must reject without posting")
        };
        assert!(matches!(result, Err(Error::DriverShutdown)));
        drop(returned);

        let mut close = Box::pin(connection.close());
        assert!(close.as_mut().poll(&mut cx).is_pending());
        drop(driver);
        assert!(
            counter.0.load(Ordering::Acquire) >= 4,
            "two operations, connection close, and shutdown must all be woken"
        );
        for operation in [&mut send, &mut recv] {
            let Poll::Ready((result, returned)) = operation.as_mut().poll(&mut cx) else {
                panic!("every in-flight operation must resolve after driver drop")
            };
            assert!(matches!(
                result,
                Err(Error::EngineWedged {
                    outstanding_operations: 2,
                    ..
                })
            ));
            assert!(returned.is_none());
        }
        assert!(matches!(
            close.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::EngineWedged {
                outstanding_operations: 2,
                ..
            }))
        ));
        assert!(matches!(
            shutdown.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::EngineWedged {
                outstanding_operations: 2,
                ..
            }))
        ));
        assert_eq!(poster.error_transitions(), 1);
        let diagnostics = engine.diagnostics();
        assert_eq!(
            diagnostics.lifecycle,
            super::super::RdmaEngineLifecycle::Failed
        );
        assert_eq!(diagnostics.quarantined_operations, 2);
        assert_eq!(diagnostics.quarantined_mrs, 2);
        assert_eq!(diagnostics.retained_cq_credits, 2);
        let released_mrs = recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count();
        assert_eq!(released_mrs, 1, "only the proven-unposted MR is released");

        drop(send);
        drop(recv);
        drop(close);
        drop(shutdown);
        drop(connection);
        drop(engine);
        assert_eq!(
            recorder
                .snapshot()
                .iter()
                .filter(|event| event.kind == DestructionKind::MemoryRegion)
                .count(),
            1,
            "accepted MRs remain in process-lifetime fail-closed ownership"
        );
        drop(recorder);
    }

    #[tokio::test]
    async fn terminal_wakers_can_reenter_after_terminal_guards_drop() {
        use futures_util::task::{ArcWake, waker};

        struct ReentrantWaker {
            shared: Arc<EngineShared>,
            wakes: AtomicUsize,
            lock_failures: AtomicUsize,
        }

        impl ArcWake for ReentrantWaker {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                arc_self.wakes.fetch_add(1, Ordering::AcqRel);
                let admission_unlocked = arc_self.shared.admission.try_write().is_ok();
                let terminal_unlocked = arc_self.shared.terminal.try_lock().is_ok();
                if admission_unlocked && terminal_unlocked {
                    let _ = arc_self.shared.diagnostics();
                } else {
                    arc_self.lock_failures.fetch_add(1, Ordering::AcqRel);
                }
            }
        }

        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 50);
        let token = install_accepted(&shared, &connection.state, WcOpcode::Send);
        let Lookup::Occupied(operation) = shared.operations.lookup(token) else {
            panic!("accepted operation")
        };
        let reentrant = Arc::new(ReentrantWaker {
            shared: Arc::clone(&shared),
            wakes: AtomicUsize::new(0),
            lock_failures: AtomicUsize::new(0),
        });
        let task_waker = waker(Arc::clone(&reentrant));
        operation.waker.register(&task_waker);
        let mut cx = Context::from_waker(&task_waker);
        let mut close = Box::pin(connection.close());
        assert!(close.as_mut().poll(&mut cx).is_pending());
        let engine = super::super::RdmaEngine {
            shared: Arc::clone(&shared),
        };
        let mut shutdown = Box::pin(engine.shutdown());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());

        shared.finish(EngineOutcome::Failure(EngineFailure::Wedged {
            retained_bundles: 1,
            outstanding_operations: 1,
            cq_debt: 1,
        }));

        assert!(
            reentrant.wakes.load(Ordering::Acquire) >= 3,
            "operation, connection, and terminal waiters must all wake"
        );
        assert_eq!(
            reentrant.lock_failures.load(Ordering::Acquire),
            0,
            "terminal wake callbacks must run after admission and terminal guards drop"
        );
        assert!(matches!(
            shutdown.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::EngineWedged { .. }))
        ));
        assert!(matches!(
            close.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::EngineWedged { .. }))
        ));
        assert_eq!(shared.quarantined_operations.load(Ordering::Acquire), 1);
        assert_eq!(shared.quarantined_mrs.load(Ordering::Acquire), 1);
        assert_eq!(shared.cq_credits.retained(), 1);
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
        assert!(operation.cancel(&shared));
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
        assert_eq!(shared.pending_reclamations.load(Ordering::Acquire), 0);
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
        assert_eq!(shared.pending_reclamations.load(Ordering::Acquire), 0);
        assert_eq!(shared.quarantined_operations.load(Ordering::Acquire), 0);
        assert_eq!(shared.quarantined_mrs.load(Ordering::Acquire), 0);
        assert_eq!(shared.quarantined_bytes.load(Ordering::Acquire), 0);
    }

    #[derive(Clone, Copy)]
    enum ScriptedPost {
        Accepted,
        Unaccepted,
        Ambiguous,
        DispatchDuringPost,
    }

    struct ScriptedPoster {
        shared: Weak<EngineShared>,
        qp_num: u32,
        outcome: ScriptedPost,
        calls: AtomicUsize,
        error_transitions: AtomicUsize,
        tokens: Mutex<Vec<OperationToken>>,
    }

    impl ScriptedPoster {
        fn new(shared: &Arc<EngineShared>, qp_num: u32, outcome: ScriptedPost) -> Self {
            Self {
                shared: Arc::downgrade(shared),
                qp_num,
                outcome,
                calls: AtomicUsize::new(0),
                error_transitions: AtomicUsize::new(0),
                tokens: Mutex::new(Vec::new()),
            }
        }

        fn post(&self, token: OperationToken, opcode: u32) -> BatchPostOutcome {
            self.calls.fetch_add(1, Ordering::AcqRel);
            lock_unpoison(&self.tokens).push(token);
            match self.outcome {
                ScriptedPost::Accepted => BatchPostOutcome::AllAccepted,
                ScriptedPost::Unaccepted => BatchPostOutcome::PrefixAccepted {
                    accepted: 0,
                    first_unaccepted: 0,
                    source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                },
                ScriptedPost::Ambiguous => BatchPostOutcome::Ambiguous {
                    source: std::io::Error::from_raw_os_error(libc::EIO),
                },
                ScriptedPost::DispatchDuringPost => {
                    let shared = self.shared.upgrade().expect("engine shared state");
                    assert_eq!(
                        shared.enqueue_completion(wc(token, self.qp_num, opcode)),
                        shared.connections.lookup_qp(self.qp_num)
                    );
                    let connection = shared
                        .connections
                        .lookup_qp(self.qp_num)
                        .expect("connection token");
                    assert_eq!(shared.process_connection_ready(connection, 1), (1, false));
                    BatchPostOutcome::AllAccepted
                }
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }

        fn error_transitions(&self) -> usize {
            self.error_transitions.load(Ordering::Acquire)
        }

        fn tokens(&self) -> Vec<OperationToken> {
            lock_unpoison(&self.tokens).clone()
        }
    }

    impl WorkRequestPoster for ScriptedPoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<crate::v2::qp::QpCapabilities> {
            None
        }

        fn post_send(&self, batch: &mut PreparedSendBatch) -> BatchPostOutcome {
            self.post(OperationToken::decode(batch.wr_id_for_test(0)), IBV_WC_SEND)
        }

        fn post_recv(&self, batch: &mut PreparedRecvBatch) -> BatchPostOutcome {
            self.post(OperationToken::decode(batch.wr_id_for_test(0)), IBV_WC_RECV)
        }

        fn to_error(&self) -> Result<()> {
            self.error_transitions.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn destroy_qp(&self) -> bool {
            false
        }

        #[cfg(any(test, feature = "test-hooks"))]
        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    fn production_engine(
        connections: usize,
        operations: usize,
        cq_capacity: usize,
    ) -> Option<(super::super::RdmaEngine, super::super::RdmaEngineDriver)> {
        let devices = crate::cm::RdmaCmDeviceList::new().ok()?;
        let device = devices
            .device_names()
            .into_iter()
            .find(|name| name.starts_with("rxe") || name.starts_with("siw"))?;
        drop(devices);
        Some(
            super::super::RdmaEngineBuilder::new(device)
                .completion_mode(super::super::CompletionMode::Polling)
                .maximum_live_connections(connections)
                .maximum_inflight_operations(operations)
                .cq_capacity(cq_capacity)
                .build()
                .expect("software-provider engine"),
        )
    }

    fn scripted_connection(
        shared: &Arc<EngineShared>,
        poster: Arc<ScriptedPoster>,
        send_wr: usize,
        recv_wr: usize,
    ) -> super::super::connection::RdmaConnection {
        install_connection(
            shared,
            poster,
            super::super::RdmaConnectionConfig::default()
                .max_send_wr(send_wr)
                .max_recv_wr(recv_wr),
            None,
            None,
        )
        .unwrap()
    }

    fn poll_once(operation: &mut RdmaOperation) -> Poll<(Result<Completion>, Option<Mr>)> {
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        Pin::new(operation).poll(&mut cx)
    }

    fn complete(
        shared: &EngineShared,
        connection: &ConnectionState,
        token: OperationToken,
        opcode: u32,
    ) {
        assert_eq!(
            shared.enqueue_completion(wc(token, connection.qp_num(), opcode)),
            Some(connection.token)
        );
        assert_eq!(
            shared.process_connection_ready(connection.token, 1),
            (1, false)
        );
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
        fn destroy_qp(&self) -> bool {
            false
        }
        #[cfg(any(test, feature = "test-hooks"))]
        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    fn synthetic_engine(capacity: usize) -> Arc<EngineShared> {
        synthetic_engine_with_hello_deadline(
            capacity,
            crate::v2::engine::config::DEFAULT_MESSAGE_HELLO_DEADLINE,
        )
    }

    fn synthetic_engine_with_hello_deadline(
        capacity: usize,
        hello_deadline: Duration,
    ) -> Arc<EngineShared> {
        let mut config = EngineConfig::new("test0".into());
        config.max_live_connections = capacity;
        config.max_inflight_operations = capacity;
        config.cq_capacity = capacity;
        config.hello_deadline = hello_deadline;
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
        synthetic_connection_on(&synthetic_engine(8), 7).into_state_without_close_for_test()
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
        let (token, operation) = shared
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
