//! Owned low-level operation futures, admission, and exact CQE routing.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::EngineShared;
use super::connection::{ConnectionState, Direction, OperationKind, QpDestructionProof};
use super::io::{
    IoEventDestination, IoEventSender, IoOperationContext, IoOperationIdentity, IoRecvRequest,
    IoSendRequest, IoSubmissionDisposition, PendingIoEvent,
};
use super::lifecycle::MemoizedTerminalResult;
use super::registry::{
    ConnectionToken, Lookup, OperationToken, PagedRegistry, lock_unpoison, read_unpoison,
};
use crate::v2::error::{Error, Result};
use crate::v2::mr::{Mr, RemoteMr};
use crate::v2::op::Completion;
use crate::v2::qp::BatchPostOutcome;
use crate::wc::{WcOpcode, WorkCompletion};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CqeReject {
    StaleConnection,
    StaleOperation,
    RetiredOperation,
    Unknown,
    Duplicate,
    WrongConnection,
    WrongQpNum,
    UnexpectedOpcode,
}

/// Future for one engine-owned SEND, RECV, READ, or WRITE.
///
/// The future owns its MR and returns
/// `(rdma_io::v2::Result<Completion>, Option<Mr>)`. Dropping it after posting
/// transfers observation to the engine; the MR, operation registration, and CQ
/// debt remain owned until the provider proves the WR unaccepted or the engine
/// consumes its exact validated success/error/flush CQE, or until synchronous
/// destruction of the owning per-connection QP proves that the HCA can no
/// longer access the MR. Timeout, QP ERR, driver loss, and CQ emptiness alone
/// are not release boundaries.
///
/// The first poll performs the synchronous `ibv_post_send` or `ibv_post_recv`
/// call. Provider posting has no wall-clock latency guarantee even though
/// completion is asynchronous.
pub struct RdmaOperation {
    state: FutureState,
}

struct InternalBatchEntry {
    token: OperationToken,
    state: Arc<OperationState>,
    sge: Sge,
}

type InternalPostInput = (Mr, Option<(usize, usize)>, IoOperationContext);

pub(super) fn post_io_recv_batch(
    shared: &Arc<EngineShared>,
    connection: &Arc<ConnectionState>,
    events: &IoEventSender,
    requests: Vec<IoRecvRequest>,
) -> IoSubmissionDisposition {
    post_io_batch(
        shared,
        connection,
        events,
        OperationKind::Recv,
        requests
            .into_iter()
            .map(|request| {
                let (mr, context) = request.into_parts();
                (mr, None, context)
            })
            .collect(),
    )
}

pub(super) fn post_io_send(
    shared: &Arc<EngineShared>,
    connection: &Arc<ConnectionState>,
    events: &IoEventSender,
    request: IoSendRequest,
) -> IoSubmissionDisposition {
    let (mr, len, context) = request.into_parts();
    post_io_batch(
        shared,
        connection,
        events,
        OperationKind::Send,
        vec![(mr, Some((0, len)), context)],
    )
}

fn post_io_batch(
    shared: &Arc<EngineShared>,
    connection: &Arc<ConnectionState>,
    events: &IoEventSender,
    kind: OperationKind,
    entries: Vec<InternalPostInput>,
) -> IoSubmissionDisposition {
    if entries.is_empty() {
        return IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: 0,
            error: Error::InvalidConfig("I/O operation batch must not be empty".into()),
        };
    }
    let count = entries.len();
    let admission = read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        let after_unlock = detach_unreserved_entries(events, entries, error.clone());
        drop(admission);
        after_unlock.publish();
        return IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: count,
            error,
        };
    }
    let posting = match connection.begin_posting() {
        Ok(posting) => posting,
        Err(error) => {
            let after_unlock = detach_unreserved_entries(events, entries, error.clone());
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
    };
    let direction = kind.direction();
    let expected_opcode = match kind {
        OperationKind::Recv => WcOpcode::Recv,
        OperationKind::Send => WcOpcode::Send,
        OperationKind::Write | OperationKind::Read => {
            let error = Error::InvalidConfig("I/O batches support only SEND and RECV".into());
            let after_unlock = detach_unreserved_entries(events, entries, error.clone());
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
    };
    let mut reserved = Vec::with_capacity(count);
    let mut entries = entries.into_iter();
    while let Some((mr, range, context)) = entries.next() {
        let validated = match ValidatedOperation::new(kind, &mr, None, range) {
            Ok(validated) => validated,
            Err(error) => {
                let mut after_unlock = rollback_internal_entries(
                    shared,
                    connection,
                    direction,
                    reserved,
                    error.clone(),
                );
                after_unlock.events.push(
                    IoEventDestination::new(events.clone(), context).unaccepted(
                        None,
                        error.clone(),
                        mr,
                    ),
                );
                after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
                drop(posting);
                drop(admission);
                after_unlock.publish();
                return IoSubmissionDisposition::FullyUnaccepted {
                    proven_unaccepted: count,
                    error,
                };
            }
        };
        if let Err(error) = connection.reserve_local(direction) {
            let mut after_unlock =
                rollback_internal_entries(shared, connection, direction, reserved, error.clone());
            after_unlock
                .events
                .push(IoEventDestination::new(events.clone(), context).unaccepted(
                    None,
                    error.clone(),
                    mr,
                ));
            after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
        let mr_len = mr.len();
        let mut mr = Some(mr);
        let mut destination = Some(IoEventDestination::new(events.clone(), context));
        let (token, state) = match shared.operations.allocate(|token| {
            Arc::new(OperationState::new_with_event(
                token,
                Arc::clone(connection),
                direction,
                expected_opcode,
                mr.take(),
                mr_len,
                destination.take(),
            ))
        }) {
            Ok(allocated) => allocated,
            Err(error) => {
                connection.release_local(direction);
                let mr = mr
                    .take()
                    .expect("operation allocation failure retains I/O MR");
                let destination = destination
                    .take()
                    .expect("operation allocation failure retains I/O destination");
                let mut after_unlock = rollback_internal_entries(
                    shared,
                    connection,
                    direction,
                    reserved,
                    error.clone(),
                );
                after_unlock
                    .events
                    .push(destination.unaccepted(None, error.clone(), mr));
                after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
                drop(posting);
                drop(admission);
                after_unlock.publish();
                return IoSubmissionDisposition::FullyUnaccepted {
                    proven_unaccepted: count,
                    error,
                };
            }
        };
        if !shared.cq_credits.reserve() {
            let error = Error::CapacityExhausted;
            let release = state
                .take_unaccepted(error.clone())
                .expect("an operation rejected before posting has no completion");
            let registered = shared
                .operations
                .release(token, false)
                .expect("unposted operation remains registered");
            debug_assert!(Arc::ptr_eq(&registered, &state));
            connection.release_local(direction);
            let mut after_unlock =
                rollback_internal_entries(shared, connection, direction, reserved, error.clone());
            if let Some(event) = release.event {
                after_unlock.events.push(event);
            }
            drop(release.mr);
            after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
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
                    let error = Error::from_v1(error);
                    let after_unlock = rollback_internal_entries(
                        shared,
                        connection,
                        direction,
                        reserved,
                        error.clone(),
                    );
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    return IoSubmissionDisposition::FullyUnaccepted {
                        proven_unaccepted: count,
                        error,
                    };
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
                    let error = Error::from_v1(error);
                    let after_unlock = rollback_internal_entries(
                        shared,
                        connection,
                        direction,
                        reserved,
                        error.clone(),
                    );
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    return IoSubmissionDisposition::FullyUnaccepted {
                        proven_unaccepted: count,
                        error,
                    };
                }
            }
        }
        OperationKind::Write | OperationKind::Read => unreachable!(),
    };
    let ownership =
        PreparedBatchOwnership::new(reserved).expect("non-empty detached batch ownership");
    let mut requests = requests;
    let outcome = match match &mut requests {
        InternalPreparedBatch::Recv(batch) => connection.poster.post_recv(batch),
        InternalPreparedBatch::Send(batch) => connection.poster.post_send(batch),
    } {
        Ok(outcome) => outcome,
        Err(error) => {
            let entries = ownership.into_entries();
            let after_unlock =
                rollback_internal_entries(shared, connection, direction, entries, error.clone());
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
    };
    let transfer = ownership.consume(outcome);
    match transfer {
        BatchOwnershipTransfer::Accepted(accepted) => {
            let after_unlock = commit_internal_entries(shared, accepted);
            drop(posting);
            drop(admission);
            after_unlock.publish();
            IoSubmissionDisposition::AllAccepted { accepted: count }
        }
        BatchOwnershipTransfer::Partial {
            mut accepted,
            unaccepted,
            source,
        } => {
            let error = Error::PostFailed(clone_io_error(&source));
            let accepted_count = accepted.len();
            let unaccepted_count = unaccepted.len();
            match release_proven_unaccepted_entries(
                shared, connection, direction, unaccepted, error,
            ) {
                InternalRelease::Released(mut after_unlock) => {
                    after_unlock.extend(commit_internal_entries(shared, accepted));
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    let error = Error::PostFailed(source);
                    if accepted_count == 0 {
                        IoSubmissionDisposition::FullyUnaccepted {
                            proven_unaccepted: unaccepted_count,
                            error,
                        }
                    } else {
                        IoSubmissionDisposition::ExactPrefix {
                            accepted: accepted_count,
                            proven_unaccepted: unaccepted_count,
                            error,
                        }
                    }
                }
                InternalRelease::Retained(mut unaccepted) => {
                    accepted.append(&mut unaccepted);
                    let after_unlock = commit_internal_entries(shared, accepted);
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    IoSubmissionDisposition::RetainedAfterEarlyCompletion {
                        retained: count,
                        error: Error::PostFailed(source),
                    }
                }
            }
        }
        BatchOwnershipTransfer::Ambiguous { retained, source } => {
            let retained_count = retained.len();
            let after_unlock = commit_internal_entries(shared, retained);
            drop(posting);
            drop(admission);
            after_unlock.publish();
            IoSubmissionDisposition::RetainedAmbiguous {
                retained: retained_count,
                error: Error::PostFailed(source),
            }
        }
    }
}

#[derive(Default)]
struct AfterEngineUnlock {
    events: Vec<PendingIoEvent>,
    operations_to_wake: Vec<Arc<OperationState>>,
}

impl AfterEngineUnlock {
    fn extend(&mut self, mut other: Self) {
        self.events.append(&mut other.events);
        self.operations_to_wake
            .append(&mut other.operations_to_wake);
    }

    fn publish(self) {
        for event in self.events {
            event.deliver();
        }
        for operation in self.operations_to_wake {
            operation.wake();
        }
    }
}

enum InternalPreparedBatch {
    Recv(PreparedRecvBatch),
    Send(PreparedSendBatch),
}

fn commit_internal_entries(
    shared: &EngineShared,
    entries: Vec<InternalBatchEntry>,
) -> AfterEngineUnlock {
    shared
        .accepted_operations
        .fetch_add(entries.len(), Ordering::AcqRel);
    let mut early = Vec::new();
    for entry in entries {
        if let Some(completion) = entry.state.commit_accepted() {
            early.push((entry.state, completion));
        }
    }
    shared.work_signal.publish(super::driver::CQ_RECHECK_WORK);
    let mut after_unlock = AfterEngineUnlock::default();
    for (state, completion) in early {
        after_unlock.extend(shared.finish_operation(state, completion));
    }
    after_unlock
}

fn rollback_internal_entries(
    shared: &EngineShared,
    connection: &ConnectionState,
    direction: Direction,
    entries: Vec<InternalBatchEntry>,
    error: Error,
) -> AfterEngineUnlock {
    match release_proven_unaccepted_entries(shared, connection, direction, entries, error) {
        InternalRelease::Released(after_unlock) => after_unlock,
        InternalRelease::Retained(entries) => {
            debug_assert!(
                entries.is_empty(),
                "an operation known not to have reached the provider acquired a completion"
            );
            commit_internal_entries(shared, entries)
        }
    }
}

enum InternalRelease {
    Released(AfterEngineUnlock),
    Retained(Vec<InternalBatchEntry>),
}

fn release_proven_unaccepted_entries(
    shared: &EngineShared,
    connection: &ConnectionState,
    direction: Direction,
    entries: Vec<InternalBatchEntry>,
    error: Error,
) -> InternalRelease {
    let releases = {
        let mut inners = entries
            .iter()
            .map(|entry| lock_unpoison(&entry.state.inner))
            .collect::<Vec<_>>();
        if inners
            .iter()
            .any(|inner| !OperationState::can_release_unaccepted(inner))
        {
            None
        } else {
            Some(
                entries
                    .iter()
                    .zip(inners.iter_mut())
                    .map(|(entry, inner)| entry.state.take_unaccepted_locked(inner, error.clone()))
                    .collect::<Vec<_>>(),
            )
        }
    };
    let Some(releases) = releases else {
        return InternalRelease::Retained(entries);
    };

    let mut after_unlock = AfterEngineUnlock::default();
    for (entry, release) in entries.into_iter().zip(releases) {
        let registered = shared
            .operations
            .release(entry.token, false)
            .expect("proven-unaccepted operation remains registered");
        debug_assert!(Arc::ptr_eq(&registered, &entry.state));
        shared.cq_credits.release();
        connection.release_local(direction);
        if let Some(event) = release.event {
            after_unlock.events.push(event);
        }
        drop(release.mr);
    }
    InternalRelease::Released(after_unlock)
}

fn detach_unreserved_entries(
    events: &IoEventSender,
    entries: impl IntoIterator<Item = InternalPostInput>,
    error: Error,
) -> AfterEngineUnlock {
    let events = entries
        .into_iter()
        .map(|(mr, _, context)| {
            IoEventDestination::new(events.clone(), context).unaccepted(None, error.clone(), mr)
        })
        .collect();
    AfterEngineUnlock {
        events,
        operations_to_wake: Vec::new(),
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
    completion: CompletionOwnership,
    output: Option<(Result<Completion>, Option<Mr>)>,
    detached: bool,
    reclamation_pending: bool,
    event_destination: Option<IoEventDestination>,
}

enum CompletionOwnership {
    None,
    // A validated CQE is owned by the connection dispatch queue.
    Queued,
    // Dispatch consumed that CQE before post reconciliation committed the WR.
    Early(WorkCompletion),
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
    pub(super) fn token(&self) -> OperationToken {
        self.token
    }

    pub(super) fn connection_token(&self) -> ConnectionToken {
        self.connection.token
    }

    fn new(
        token: OperationToken,
        connection: Arc<ConnectionState>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
    ) -> Self {
        Self::new_with_event(
            token,
            connection,
            direction,
            expected_opcode,
            mr,
            mr_len,
            None,
        )
    }

    fn new_with_event(
        token: OperationToken,
        connection: Arc<ConnectionState>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
        event_destination: Option<IoEventDestination>,
    ) -> Self {
        let detached = event_destination.is_some();
        Self {
            token,
            connection,
            direction,
            expected_opcode,
            mr_len,
            inner: Mutex::new(OperationInner {
                lifecycle: OperationLifecycle::Posting,
                mr,
                completion: CompletionOwnership::None,
                output: None,
                detached,
                reclamation_pending: false,
                event_destination,
            }),
            waker: AtomicWaker::new(),
            cancelled: AtomicBool::new(false),
            quarantined: AtomicBool::new(false),
        }
    }

    fn commit_accepted(&self) -> Option<WorkCompletion> {
        let mut inner = lock_unpoison(&self.inner);
        self.connection.add_accepted(self.token);
        let accepted_lifecycle = if inner.detached {
            OperationLifecycle::Cancelled
        } else {
            OperationLifecycle::InFlight
        };
        match std::mem::replace(&mut inner.completion, CompletionOwnership::None) {
            CompletionOwnership::None => {
                inner.lifecycle = accepted_lifecycle;
                None
            }
            CompletionOwnership::Queued => {
                inner.completion = CompletionOwnership::Queued;
                inner.lifecycle = accepted_lifecycle;
                None
            }
            CompletionOwnership::Early(completion) => {
                inner.lifecycle = OperationLifecycle::Completing;
                Some(completion)
            }
        }
    }

    fn mark_completion_queued(&self) -> bool {
        let mut inner = lock_unpoison(&self.inner);
        if matches!(
            inner.lifecycle,
            OperationLifecycle::Completing | OperationLifecycle::Released
        ) || !matches!(inner.completion, CompletionOwnership::None)
        {
            return false;
        }
        inner.completion = CompletionOwnership::Queued;
        true
    }

    fn record_completion(&self, completion: WorkCompletion) -> CompletionDisposition {
        let mut inner = lock_unpoison(&self.inner);
        if !matches!(inner.completion, CompletionOwnership::Queued) {
            return CompletionDisposition::Duplicate;
        }
        inner.completion = CompletionOwnership::None;
        match inner.lifecycle {
            OperationLifecycle::Posting => {
                inner.completion = CompletionOwnership::Early(completion);
                CompletionDisposition::Deferred
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
        let typed = Completion::from_raw(completion);
        let result = typed.result().map(|()| typed);
        let event = inner.event_destination.take().map(|destination| {
            let event_mr = mr.take();
            destination.complete(
                IoOperationIdentity::from_token(self.token),
                result.clone(),
                event_mr,
            )
        });
        let detached_mr = if event.is_some() {
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
        FinishState {
            was_reclaiming,
            was_quarantined,
            event,
        }
    }

    fn finish_after_qp_destroy(&self, error: Error) -> FinishState {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        inner.reclamation_pending = false;
        let was_quarantined = self.quarantined.swap(false, Ordering::AcqRel);
        let mut mr = inner.mr.take();
        let event = inner.event_destination.take().map(|destination| {
            let event_mr = mr.take();
            destination.complete(
                IoOperationIdentity::from_token(self.token),
                Err(error.clone()),
                event_mr,
            )
        });
        if event.is_none() && !inner.detached && inner.output.is_none() {
            inner.output = Some((Err(error), None));
        }
        inner.lifecycle = OperationLifecycle::Released;
        drop(inner);
        drop(mr);
        FinishState {
            was_reclaiming,
            was_quarantined,
            event,
        }
    }

    fn take_mr(&self) -> Option<Mr> {
        lock_unpoison(&self.inner).mr.take()
    }

    fn take_unaccepted(&self, error: Error) -> Option<UnacceptedRelease> {
        let mut inner = lock_unpoison(&self.inner);
        if !Self::can_release_unaccepted(&inner) {
            return None;
        }
        Some(self.take_unaccepted_locked(&mut inner, error))
    }

    fn can_release_unaccepted(inner: &OperationInner) -> bool {
        inner.lifecycle == OperationLifecycle::Posting
            && matches!(inner.completion, CompletionOwnership::None)
    }

    fn take_unaccepted_locked(
        &self,
        inner: &mut OperationInner,
        error: Error,
    ) -> UnacceptedRelease {
        debug_assert!(Self::can_release_unaccepted(inner));
        inner.lifecycle = OperationLifecycle::Released;
        let mut mr = inner.mr.take();
        let event = inner.event_destination.take().map(|destination| {
            destination.unaccepted(
                Some(IoOperationIdentity::from_token(self.token)),
                error,
                mr.take().expect("unaccepted I/O operation retains its MR"),
            )
        });
        UnacceptedRelease { event, mr }
    }

    #[cfg(test)]
    fn can_release_unaccepted_for_test(&self) -> bool {
        let inner = lock_unpoison(&self.inner);
        Self::can_release_unaccepted(&inner)
    }

    #[cfg(test)]
    fn completion_ownership_for_test(&self) -> &'static str {
        match lock_unpoison(&self.inner).completion {
            CompletionOwnership::None => "none",
            CompletionOwnership::Queued => "queued",
            CompletionOwnership::Early(_) => "early",
        }
    }

    fn take_output(&self) -> Option<(Result<Completion>, Option<Mr>)> {
        lock_unpoison(&self.inner).output.take()
    }

    fn detach_with_post_error(&self, shared: &EngineShared) {
        let mut inner = lock_unpoison(&self.inner);
        inner.detached = true;
        inner.lifecycle = OperationLifecycle::Cancelled;
        shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
        inner.reclamation_pending = true;
        self.cancelled.store(true, Ordering::Release);
    }

    pub(super) fn finalize_terminal(&self, outcome: &MemoizedTerminalResult) -> TerminalizeState {
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
            let error = outcome.error().unwrap_or(Error::DriverShutdown);
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

struct UnacceptedRelease {
    event: Option<PendingIoEvent>,
    mr: Option<Mr>,
}

struct FinishState {
    was_reclaiming: bool,
    was_quarantined: bool,
    event: Option<PendingIoEvent>,
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

    fn into_entries(self) -> Vec<T> {
        self.entries
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
            return StartResult::Immediate((Err(error), mr));
        }
    };

    if !shared.cq_credits.reserve() {
        let state = shared.operations.release(token, false).unwrap_or(state);
        connection.release_local(direction);
        return StartResult::Immediate((Err(Error::CapacityExhausted), state.take_mr()));
    }
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
            shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
            let early = state.commit_accepted();
            shared.work_signal.publish(super::driver::CQ_RECHECK_WORK);
            drop(admission);
            if let Some(completion) = early {
                shared
                    .finish_operation(Arc::clone(&state), completion)
                    .publish();
            }
            StartResult::InFlight(state)
        }
        BatchPostOutcome::PrefixAccepted {
            accepted,
            first_unaccepted,
            source,
        } if accepted == 0 && first_unaccepted == 0 => {
            let error = Error::PostFailed(source);
            if let Some(release) = state.take_unaccepted(error.clone()) {
                let registered = shared
                    .operations
                    .release(token, false)
                    .expect("proven-unaccepted operation remains registered");
                debug_assert!(Arc::ptr_eq(&registered, &state));
                shared.cq_credits.release();
                connection.release_local(direction);
                debug_assert!(release.event.is_none());
                drop(release.event);
                StartResult::Immediate((Err(error), release.mr))
            } else {
                shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
                let early = state.commit_accepted();
                shared.work_signal.publish(super::driver::CQ_RECHECK_WORK);
                drop(admission);
                if let Some(completion) = early {
                    shared
                        .finish_operation(Arc::clone(&state), completion)
                        .publish();
                }
                StartResult::InFlight(state)
            }
        }
        BatchPostOutcome::PrefixAccepted { source, .. }
        | BatchPostOutcome::Ambiguous { source } => {
            shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
            let early = state.commit_accepted();
            shared.work_signal.publish(super::driver::CQ_RECHECK_WORK);
            drop(admission);
            if let Some(completion) = early {
                shared
                    .finish_operation(Arc::clone(&state), completion)
                    .publish();
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
                        .map_err(Error::from_v1)?;
                connection.poster.post_recv(&mut batch)
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
                let mut batch = PreparedSendBatch::new(vec![wr]).map_err(Error::from_v1)?;
                connection.poster.post_send(&mut batch)
            }
        }
    }
}

impl EngineShared {
    fn reject_cqe(&self, reason: CqeReject) {
        #[cfg(any(test, feature = "test-hooks"))]
        {
            self.rejected_cqes.fetch_add(1, Ordering::Relaxed);
            lock_unpoison(&self.rejected_cqe_reasons).push(reason);
        }
        #[cfg(not(any(test, feature = "test-hooks")))]
        let _ = reason;
    }

    pub(super) fn enqueue_completion(&self, completion: WorkCompletion) -> Option<ConnectionToken> {
        let _admission = read_unpoison(&self.admission);
        let token = OperationToken::decode(completion.wr_id());
        let operation = match self.operations.lookup(token) {
            Lookup::Occupied(operation) => operation,
            Lookup::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                return None;
            }
            Lookup::Stale => {
                self.reject_cqe(CqeReject::StaleOperation);
                return None;
            }
            Lookup::Retired => {
                self.reject_cqe(CqeReject::RetiredOperation);
                return None;
            }
            Lookup::Unknown => {
                self.reject_cqe(CqeReject::Unknown);
                return None;
            }
        };
        let connection = match self.connections.lookup(operation.connection.token) {
            Lookup::Occupied(connection) => connection,
            _ => {
                self.reject_cqe(CqeReject::StaleConnection);
                return None;
            }
        };
        if completion.qp_num() != operation.connection.qp_num() {
            self.reject_cqe(CqeReject::WrongQpNum);
            return None;
        }
        if self.connections.lookup_qp(completion.qp_num()) != Some(operation.connection.token) {
            self.reject_cqe(CqeReject::WrongConnection);
            return None;
        }
        if completion.is_success() && completion.opcode() != operation.expected_opcode {
            self.reject_cqe(CqeReject::UnexpectedOpcode);
            return None;
        }
        if !operation.mark_completion_queued() {
            self.reject_cqe(CqeReject::Duplicate);
            return None;
        }
        connection.enqueue_completion(completion);
        Some(connection.token)
    }

    pub(super) fn dispatch_connection_completions(
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
                self.dispatch_connection_completion(completion);
                processed += 1;
                continue;
            }
            break;
        }
        (processed, connection.has_completion_work())
    }

    fn dispatch_queued_completion(&self, completion: WorkCompletion) -> AfterEngineUnlock {
        let token = OperationToken::decode(completion.wr_id());
        let operation = match self.operations.lookup(token) {
            Lookup::Occupied(operation) => operation,
            Lookup::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                return AfterEngineUnlock::default();
            }
            Lookup::Stale => {
                self.reject_cqe(CqeReject::StaleOperation);
                return AfterEngineUnlock::default();
            }
            Lookup::Retired => {
                self.reject_cqe(CqeReject::RetiredOperation);
                return AfterEngineUnlock::default();
            }
            Lookup::Unknown => {
                self.reject_cqe(CqeReject::Unknown);
                return AfterEngineUnlock::default();
            }
        };
        match operation.record_completion(completion) {
            CompletionDisposition::Deferred => AfterEngineUnlock::default(),
            CompletionDisposition::Complete => self.finish_operation(operation, completion),
            CompletionDisposition::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                AfterEngineUnlock::default()
            }
        }
    }

    fn finish_operation(
        &self,
        operation: Arc<OperationState>,
        completion: WorkCompletion,
    ) -> AfterEngineUnlock {
        if self.operations.release(operation.token, true).is_none() {
            self.reject_cqe(CqeReject::Duplicate);
            return AfterEngineUnlock::default();
        }
        let removed = operation.connection.remove_accepted(operation.token);
        operation.connection.release_local(operation.direction);
        self.cq_credits.release();
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
            self.clear_operation_quarantine(&operation);
        }
        let event = finished.event;
        if removed
            && operation.connection.close_started()
            && operation.connection.accepted_count() == 0
        {
            self.recover_connection_quarantine(&operation.connection);
            self.record_connection_drained(&operation.connection);
            self.schedule_connection_retirement(&operation.connection);
        }
        AfterEngineUnlock {
            events: event.into_iter().collect(),
            operations_to_wake: vec![operation],
        }
    }

    pub(super) fn reclaim_after_qp_destroy(
        &self,
        proof: &QpDestructionProof,
        connection: &ConnectionState,
        token: OperationToken,
    ) -> bool {
        if !proof.proves(connection) {
            tracing::warn!(
                connection = connection.token.encode(),
                operation = token.encode(),
                "operation reclaim rejected a mismatched QP destruction proof"
            );
            return false;
        }
        let Lookup::Occupied(operation) = self.operations.lookup(token) else {
            tracing::warn!(
                connection = connection.token.encode(),
                operation = token.encode(),
                "QP destruction left an accepted token without an operation registration"
            );
            return false;
        };
        if operation.connection_token() != connection.token {
            tracing::warn!(
                connection = connection.token.encode(),
                operation = token.encode(),
                owner = operation.connection_token().encode(),
                "QP destruction found an accepted token owned by another connection"
            );
            return false;
        }
        if !connection.remove_accepted(token) {
            tracing::warn!(
                connection = connection.token.encode(),
                operation = token.encode(),
                "QP destruction reclaim lost accepted-set membership"
            );
            return false;
        }
        if self.operations.release(token, false).is_none() {
            connection.add_accepted(token);
            tracing::warn!(
                connection = connection.token.encode(),
                operation = token.encode(),
                "QP destruction reclaim could not retire the operation registration"
            );
            return false;
        }
        connection.release_local(operation.direction);
        self.cq_credits.release();
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        if previous == 1 && self.shutdown_requested.load(Ordering::Acquire) {
            self.work_signal.publish(super::driver::TERMINAL_WORK);
        }
        let finished = operation.finish_after_qp_destroy(connection.operation_close_error());
        if finished.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_mrs.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_bytes
                .fetch_sub(operation.mr_len, Ordering::AcqRel);
            self.clear_operation_quarantine(&operation);
        }
        AfterEngineUnlock {
            events: finished.event.into_iter().collect(),
            operations_to_wake: vec![operation],
        }
        .publish();
        true
    }

    fn dispatch_connection_completion(&self, completion: WorkCompletion) {
        // CQ routing and terminal publication share the admission barrier.
        // The lock covers only one completion and is released before an event
        // enqueue or operation wake can re-enter posting or close.
        let after_unlock = {
            let _admission = read_unpoison(&self.admission);
            self.dispatch_queued_completion(completion)
        };
        after_unlock.publish();
    }

    fn dispatch_queued_completions(&self, connection: &ConnectionState, budget: usize) -> bool {
        for _ in 0..budget {
            let Some(completion) = connection.pop_completion() else {
                return false;
            };
            self.dispatch_connection_completion(completion);
        }
        connection.has_completion_work()
    }

    pub(super) fn reject_queued_completions_after_qp_destroy(
        &self,
        connection: &ConnectionState,
    ) -> bool {
        // The sole driver invokes this after the destruction boundary. CQEs
        // queued before the boundary were already dispatched normally; only
        // completions arriving after that boundary are rejected as stale.
        self.dispatch_queued_completions(connection, self.config.completion_dispatch_budget)
    }

    pub(super) fn begin_reclamation(&self, token: OperationToken) {
        if let Lookup::Occupied(operation) = self.operations.lookup(token) {
            operation.mark_reclaiming();
        }
    }

    pub(super) fn handle_reclamation_deadline(&self, token: OperationToken) {
        self.quarantine_operation(token);
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
        self.track_operation_quarantine(&operation);
        true
    }
}

#[cfg(test)]
pub(super) fn install_accepted_operation_for_driver_test(
    shared: &Arc<EngineShared>,
    connection: &Arc<ConnectionState>,
    opcode: WcOpcode,
) -> OperationToken {
    let direction = if opcode == WcOpcode::Recv {
        Direction::Recv
    } else {
        Direction::Send
    };
    connection.reserve_local(direction).unwrap();
    assert!(shared.cq_credits.reserve());
    let (token, operation) = shared
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(connection),
                direction,
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

#[cfg(test)]
pub(super) fn completion_for_driver_test(
    token: OperationToken,
    qp_num: u32,
    opcode: u32,
    status: u32,
) -> WorkCompletion {
    let mut completion = WorkCompletion::default();
    completion.inner.wr_id = token.encode();
    completion.inner.qp_num = qp_num;
    completion.inner.opcode = opcode;
    completion.inner.status = status;
    completion
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::destruction::{DestructionKind, DestructionRecorder};
    use crate::v2::AccessIntent;
    use crate::v2::engine::config::EngineConfig;
    use crate::v2::engine::connection::{WorkRequestPoster, install_connection};
    use crate::v2::engine::lifecycle::MemoizedTerminalResult;
    use crate::wc::WcStatus;
    use rdma_io_sys::ibverbs::{IBV_WC_FATAL_ERR, IBV_WC_RECV, IBV_WC_SEND, IBV_WC_SUCCESS};
    use std::sync::Barrier;
    use std::sync::Weak;

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
        assert!(operation.mark_completion_queued());
        assert!(matches!(
            operation.record_completion(completion),
            CompletionDisposition::Deferred
        ));
        assert_eq!(operation.lifecycle(), OperationLifecycle::Posting);
        let early = operation.commit_accepted().expect("early completion");
        shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
        assert_eq!(operation.lifecycle(), OperationLifecycle::Completing);
        shared
            .finish_operation(Arc::clone(&operation), early)
            .publish();
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
        assert!(operation.mark_completion_queued());
        assert!(matches!(
            operation.record_completion(completion),
            CompletionDisposition::Complete
        ));
        assert_eq!(operation.lifecycle(), OperationLifecycle::Completing);
        shared
            .finish_operation(Arc::clone(&operation), completion)
            .publish();
        assert_eq!(operation.lifecycle(), OperationLifecycle::Released);
    }

    #[test]
    fn io_completion_wakes_after_admission_guard_is_released() {
        use std::task::{Wake, Waker};

        struct AdmissionCheckingWake {
            shared: Arc<EngineShared>,
            observed: AtomicBool,
        }

        impl Wake for AdmissionCheckingWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                assert!(
                    self.shared.admission.try_write().is_ok(),
                    "I/O event wake ran while admission remained locked"
                );
                self.observed.store(true, Ordering::Release);
            }
        }

        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 21);
        connection.state.reserve_local(Direction::Send).unwrap();
        assert!(shared.cq_credits.reserve());
        let (sender, receiver) = super::super::io::event_port();
        let wake = Arc::new(AdmissionCheckingWake {
            shared: Arc::clone(&shared),
            observed: AtomicBool::new(false),
        });
        receiver.register(&Waker::from(Arc::clone(&wake)));
        let (token, operation) = shared
            .operations
            .allocate(|token| {
                Arc::new(OperationState::new_with_event(
                    token,
                    Arc::clone(&connection.state),
                    Direction::Send,
                    WcOpcode::Send,
                    None,
                    1,
                    Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
                ))
            })
            .unwrap();
        connection.state.add_accepted(token);
        operation.commit_accepted();
        shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
        assert_eq!(
            shared.enqueue_completion(wc(token, connection.identity().qp_num(), IBV_WC_SEND,)),
            Some(connection.state.token)
        );

        assert_eq!(
            shared.dispatch_connection_completions(connection.state.token, 1),
            (1, false)
        );
        assert!(wake.observed.load(Ordering::Acquire));
        assert!(matches!(
            receiver.pop(),
            Some(super::super::io::IoEvent::Completion(_))
        ));
    }

    #[test]
    fn qp_destroy_event_uses_the_contextual_connection_close_error() {
        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 18);
        connection.state.reserve_local(Direction::Send).unwrap();
        assert!(shared.cq_credits.reserve());
        let (sender, receiver) = super::super::io::event_port();
        let (token, operation) = shared
            .operations
            .allocate(|token| {
                Arc::new(OperationState::new_with_event(
                    token,
                    Arc::clone(&connection.state),
                    Direction::Send,
                    WcOpcode::Send,
                    None,
                    1,
                    Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
                ))
            })
            .unwrap();
        operation.commit_accepted();
        shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
        let terminal = connection
            .state
            .mark_cm_failure(Error::ProtocolViolation("contextual close failure".into()));
        drop(terminal);
        let proof = connection.state.mint_qp_destruction_proof_for_test();

        assert!(shared.reclaim_after_qp_destroy(&proof, &connection.state, token));
        let Some(super::super::io::IoEvent::Completion(completion)) = receiver.pop() else {
            panic!("QP destruction must publish an owned completion event")
        };
        let (_, _, result, _, unaccepted) = completion.into_parts();
        assert!(!unaccepted);
        assert!(matches!(
            result,
            Err(Error::ProtocolViolation(message)) if message == "contextual close failure"
        ));
    }

    #[test]
    fn unresolved_reclamation_requires_the_exact_connection_destruction_proof() {
        let shared = synthetic_engine(8);
        let owner = synthetic_connection_on(&shared, 22);
        let other = synthetic_connection_on(&shared, 23);
        let token = install_accepted(&shared, &owner.state, WcOpcode::Send);
        let wrong_proof = other.state.mint_qp_destruction_proof_for_test();

        assert!(!shared.reclaim_after_qp_destroy(&wrong_proof, &owner.state, token));
        assert_eq!(owner.state.accepted_count(), 1);
        assert!(matches!(
            shared.operations.lookup(token),
            Lookup::Occupied(_)
        ));

        let proof = owner.state.mint_qp_destruction_proof_for_test();
        assert!(shared.reclaim_after_qp_destroy(&proof, &owner.state, token));
        assert_eq!(owner.state.accepted_count(), 0);
        assert_eq!(shared.operations.live(), 0);
    }

    #[test]
    fn exact_routing_rejects_invalid_classes_and_delivers_fatal_statuses() {
        let shared = synthetic_engine(8);
        let first = synthetic_connection_on(&shared, 7);
        let exact = install_accepted(&shared, &first.state, WcOpcode::Send);
        let exact_wc = wc(exact, 7, IBV_WC_SEND);
        assert_eq!(shared.enqueue_completion(exact_wc), Some(first.state.token));
        assert_eq!(
            shared.dispatch_connection_completions(first.state.token, 1),
            (1, false)
        );

        for (raw_status, expected_status) in [
            (IBV_WC_FATAL_ERR, WcStatus::FatalErr),
            (u32::MAX, WcStatus::Unknown(u32::MAX)),
        ] {
            let (fatal, events) =
                install_accepted_with_result(&shared, &first.state, WcOpcode::Send);
            let mut fatal_wc = wc(fatal, 7, IBV_WC_SEND);
            fatal_wc.inner.status = raw_status;
            assert_eq!(shared.enqueue_completion(fatal_wc), Some(first.state.token));
            assert_eq!(
                shared.dispatch_connection_completions(first.state.token, 1),
                (1, false)
            );
            let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
                panic!("fatal completion event")
            };
            let (_, _, result, mr, unaccepted) = completion.into_parts();
            assert!(mr.is_none());
            assert!(!unaccepted);
            assert!(matches!(
                result,
                Err(Error::CompletionError { status, vendor_err: 0 })
                    if status == expected_status
            ));
        }
        assert_eq!(shared.rejected_cqes.load(Ordering::Acquire), 0);

        assert!(shared.enqueue_completion(exact_wc).is_none());
        assert_eq!(shared.rejected_cqes.load(Ordering::Acquire), 1);

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

        let (retired, _) = shared
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
        let retired = shared
            .operations
            .slots
            .force_generation_for_test(retired, u32::MAX);
        shared.operations.release(retired, false).unwrap();
        assert!(matches!(shared.operations.lookup(retired), Lookup::Retired));
        assert!(
            shared
                .enqueue_completion(wc(retired, 7, IBV_WC_SEND))
                .is_none()
        );
        assert_eq!(
            lock_unpoison(&shared.rejected_cqe_reasons).last(),
            Some(&CqeReject::RetiredOperation)
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

        assert_eq!(shared.rejected_cqes.load(Ordering::Acquire), 8);
        let rejection_reasons = lock_unpoison(&shared.rejected_cqe_reasons);
        assert!(rejection_reasons.contains(&CqeReject::StaleOperation));
        assert!(rejection_reasons.contains(&CqeReject::RetiredOperation));
    }

    #[test]
    fn queued_completion_marker_rejects_duplicates_before_dispatch() {
        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 16);
        let token = install_accepted(&shared, &connection.state, WcOpcode::Send);
        let completion = wc(token, 16, IBV_WC_SEND);

        assert_eq!(
            shared.enqueue_completion(completion),
            Some(connection.state.token)
        );
        assert!(shared.enqueue_completion(completion).is_none());
        assert_eq!(shared.rejected_cqes.load(Ordering::Acquire), 1);
        assert_eq!(
            lock_unpoison(&shared.rejected_cqe_reasons).as_slice(),
            &[CqeReject::Duplicate]
        );
        assert_eq!(
            shared.dispatch_connection_completions(connection.state.token, 2),
            (1, false)
        );
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 0);
        assert_eq!(shared.cq_credits.free(), 8);
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
    fn completion_dispatch_budget_bounds_routed_work_without_idle_scans() {
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
            shared.dispatch_connection_completions(connection.state.token, 2),
            (2, true)
        );
        assert_eq!(
            shared.dispatch_connection_completions(connection.state.token, 2),
            (1, false)
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
        drop(returned);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn empty_and_zero_accepted_io_batches_reject_and_roll_back_every_reservation() {
        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 51, ScriptedPost::Unaccepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 2);
        let (io, events) =
            super::super::io::IoConnection::new(Arc::clone(&shared), Arc::clone(&connection.state))
                .unwrap();

        assert!(matches!(
            io.post_recv_batch(Vec::new()),
            IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: 0,
                error: Error::InvalidConfig(_)
            }
        ));
        assert_eq!(poster.calls(), 0);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);

        let recorder = DestructionRecorder::arm(8);
        let mut entries = Vec::new();
        for _ in 0..2 {
            let mr = shared.register_memory(64, AccessIntent::LocalOnly).unwrap();
            entries.push(IoRecvRequest::new(mr, IoOperationContext::new(())));
        }
        assert!(matches!(
            io.post_recv_batch(entries),
            IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: 2,
                error: Error::PostFailed(_)
            }
        ));
        assert_eq!(events.queued_len(), 2);
        assert_eq!(poster.calls(), 1);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        assert_eq!(connection.state.accepted_count(), 0);
        connection.state.reserve_local(Direction::Recv).unwrap();
        connection.state.reserve_local(Direction::Recv).unwrap();
        connection.state.release_local(Direction::Recv);
        connection.state.release_local(Direction::Recv);
        drop(events.drain());
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
    fn exact_prefix_returns_only_the_proven_unaccepted_suffix() {
        let Some((engine, driver)) = production_engine(2, 8, 8) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(
            &shared,
            53,
            ScriptedPost::PrefixAccepted(1),
        ));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 3);
        let (io, events) =
            super::super::io::IoConnection::new(Arc::clone(&shared), Arc::clone(&connection.state))
                .unwrap();
        let requests = (0usize..3)
            .map(|context| {
                IoRecvRequest::new(
                    io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
                    IoOperationContext::new(context),
                )
            })
            .collect();

        assert!(matches!(
            io.post_recv_batch(requests),
            IoSubmissionDisposition::ExactPrefix {
                accepted: 1,
                proven_unaccepted: 2,
                error: Error::PostFailed(_)
            }
        ));
        let mut rejected = Vec::new();
        for _ in 0..2 {
            let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
                panic!("proven-unaccepted suffix event")
            };
            let (_, context, result, mr, unaccepted) = completion.into_parts();
            rejected.push(context.downcast::<usize>().ok().unwrap());
            assert!(matches!(result, Err(Error::PostFailed(_))));
            assert!(mr.is_some());
            assert!(unaccepted);
        }
        rejected.sort_unstable();
        assert_eq!(rejected, vec![1, 2]);
        assert_eq!(connection.state.accepted_count(), 1);
        complete(&shared, &connection.state, poster.tokens()[0], IBV_WC_RECV);
        assert!(matches!(
            events.pop(),
            Some(super::super::io::IoEvent::Completion(_))
        ));
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 8);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn exact_prefix_with_early_suffix_cqe_retains_the_entire_batch() {
        let Some((engine, driver)) = production_engine(2, 8, 8) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(
            &shared,
            54,
            ScriptedPost::PrefixWithSuffixCompletion {
                accepted: 1,
                completed_suffix: 1,
            },
        ));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 3);
        let (io, events) =
            super::super::io::IoConnection::new(Arc::clone(&shared), Arc::clone(&connection.state))
                .unwrap();
        let requests = (0usize..3)
            .map(|context| {
                IoRecvRequest::new(
                    io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
                    IoOperationContext::new(context),
                )
            })
            .collect();

        assert!(matches!(
            io.post_recv_batch(requests),
            IoSubmissionDisposition::RetainedAfterEarlyCompletion {
                retained: 3,
                error: Error::PostFailed(_)
            }
        ));
        let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
            panic!("early suffix completion event")
        };
        let (_, context, result, mr, unaccepted) = completion.into_parts();
        assert_eq!(context.downcast::<usize>().ok().unwrap(), 1);
        assert!(result.is_ok());
        assert!(mr.is_some());
        assert!(!unaccepted);
        assert!(!events.has_events());
        assert_eq!(connection.state.accepted_count(), 2);
        assert_eq!(shared.operations.live(), 2);

        let tokens = poster.tokens();
        complete(&shared, &connection.state, tokens[0], IBV_WC_RECV);
        complete(&shared, &connection.state, tokens[2], IBV_WC_RECV);
        assert_eq!(events.drain().len(), 2);
        assert_eq!(connection.state.accepted_count(), 0);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 8);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn exact_prefix_with_queued_suffix_cqe_retains_the_entire_batch_until_dispatch() {
        let Some((engine, driver)) = production_engine(2, 8, 8) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(
            &shared,
            55,
            ScriptedPost::PrefixWithQueuedSuffixCompletion {
                accepted: 1,
                completed_suffix: 2,
            },
        ));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 3);
        let (io, events) =
            super::super::io::IoConnection::new(Arc::clone(&shared), Arc::clone(&connection.state))
                .unwrap();
        let requests = (0usize..3)
            .map(|context| {
                IoRecvRequest::new(
                    io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
                    IoOperationContext::new(context),
                )
            })
            .collect();

        assert!(matches!(
            io.post_recv_batch(requests),
            IoSubmissionDisposition::RetainedAfterEarlyCompletion {
                retained: 3,
                error: Error::PostFailed(_)
            }
        ));
        assert!(!events.has_events());
        assert_eq!(connection.state.accepted_count(), 3);
        assert_eq!(shared.operations.live(), 3);
        assert_eq!(shared.cq_credits.free(), 5);

        assert_eq!(
            shared.dispatch_connection_completions(connection.state.token, 1),
            (1, false)
        );
        let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
            panic!("queued suffix completion event")
        };
        let (_, context, result, mr, unaccepted) = completion.into_parts();
        assert_eq!(context.downcast::<usize>().ok().unwrap(), 2);
        assert!(result.is_ok());
        assert!(mr.is_some());
        assert!(!unaccepted);
        assert_eq!(connection.state.accepted_count(), 2);
        assert_eq!(shared.operations.live(), 2);

        let tokens = poster.tokens();
        complete(&shared, &connection.state, tokens[0], IBV_WC_RECV);
        complete(&shared, &connection.state, tokens[1], IBV_WC_RECV);
        assert_eq!(events.drain().len(), 2);
        assert_eq!(connection.state.accepted_count(), 0);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.cq_credits.free(), 8);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn dispatch_between_releasability_observation_and_release_retains_the_whole_suffix() {
        let shared = synthetic_engine(8);
        let connection = synthetic_connection_on(&shared, 56);
        let mut entries = Vec::new();
        for _ in 0..2 {
            connection.state.reserve_local(Direction::Recv).unwrap();
            assert!(shared.cq_credits.reserve());
            let (token, state) = shared
                .operations
                .allocate(|token| {
                    Arc::new(OperationState::new(
                        token,
                        Arc::clone(&connection.state),
                        Direction::Recv,
                        WcOpcode::Recv,
                        None,
                        1,
                    ))
                })
                .unwrap();
            entries.push(InternalBatchEntry {
                token,
                state,
                sge: Sge::new(0, 0, 0),
            });
        }
        assert!(
            entries
                .iter()
                .all(|entry| entry.state.can_release_unaccepted_for_test())
        );

        let raced = entries[1].token;
        assert_eq!(
            shared.enqueue_completion(wc(raced, 56, IBV_WC_RECV)),
            Some(connection.state.token)
        );
        assert_eq!(entries[1].state.completion_ownership_for_test(), "queued");
        assert_eq!(
            shared.dispatch_connection_completions(connection.state.token, 1),
            (1, false)
        );
        assert_eq!(entries[1].state.completion_ownership_for_test(), "early");

        let entries = match release_proven_unaccepted_entries(
            &shared,
            &connection.state,
            Direction::Recv,
            entries,
            Error::PostFailed(std::io::Error::from_raw_os_error(libc::ENOMEM)),
        ) {
            InternalRelease::Retained(entries) => entries,
            InternalRelease::Released(_) => {
                panic!("a recorded suffix CQE must prevent every suffix release")
            }
        };
        assert_eq!(shared.operations.live(), 2);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 0);
        assert_eq!(shared.cq_credits.free(), 6);
        connection.state.reserve_local(Direction::Recv).unwrap();
        connection.state.reserve_local(Direction::Recv).unwrap();
        assert!(matches!(
            connection.state.reserve_local(Direction::Recv),
            Err(Error::CapacityExhausted)
        ));
        connection.state.release_local(Direction::Recv);
        connection.state.release_local(Direction::Recv);

        commit_internal_entries(&shared, entries).publish();
        assert_eq!(shared.operations.live(), 1);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 1);
        assert_eq!(connection.state.accepted_count(), 1);
        assert_eq!(shared.cq_credits.free(), 7);

        let remaining = connection.state.accepted_tokens();
        assert_eq!(remaining.len(), 1);
        complete(&shared, &connection.state, remaining[0], IBV_WC_RECV);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 0);
        assert_eq!(connection.state.accepted_count(), 0);
        assert_eq!(shared.cq_credits.free(), 8);
        for _ in 0..4 {
            connection.state.reserve_local(Direction::Recv).unwrap();
        }
        assert!(matches!(
            connection.state.reserve_local(Direction::Recv),
            Err(Error::CapacityExhausted)
        ));
        for _ in 0..4 {
            connection.state.release_local(Direction::Recv);
        }
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
        drop(returned);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn io_early_completion_event_is_published_after_post_guards_are_released() {
        use std::task::{Wake, Waker};

        struct PostGuardCheckingWake {
            shared: Arc<EngineShared>,
            connection: Arc<ConnectionState>,
            observed: AtomicBool,
        }

        impl Wake for PostGuardCheckingWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                assert!(self.shared.admission.try_write().is_ok());
                assert!(self.connection.begin_posting().is_ok());
                self.observed.store(true, Ordering::Release);
            }
        }

        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(
            &shared,
            49,
            ScriptedPost::DispatchDuringPost,
        ));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let (io, events) =
            super::super::io::IoConnection::new(Arc::clone(&shared), Arc::clone(&connection.state))
                .unwrap();
        let wake = Arc::new(PostGuardCheckingWake {
            shared: Arc::clone(&shared),
            connection: Arc::clone(&connection.state),
            observed: AtomicBool::new(false),
        });
        events.register(&Waker::from(Arc::clone(&wake)));
        let posted = io.post_send(IoSendRequest::new(
            io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            1,
            IoOperationContext::new(()),
        ));
        assert!(posted.all_accepted(), "I/O early-completion post failed");
        assert!(wake.observed.load(Ordering::Acquire));
        let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
            panic!("early completion event")
        };
        let (_, _, result, mr, unaccepted) = completion.into_parts();
        assert!(result.is_ok());
        assert!(mr.is_some());
        assert!(!unaccepted);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 0);
        assert_eq!(shared.cq_credits.free(), 4);
        drop(connection);
        drop(driver);
        drop(engine);
    }

    #[test]
    fn io_unaccepted_event_is_published_after_post_guards_are_released() {
        use std::task::{Wake, Waker};

        struct PostGuardCheckingWake {
            shared: Arc<EngineShared>,
            connection: Arc<ConnectionState>,
            observed: AtomicBool,
        }

        impl Wake for PostGuardCheckingWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                assert!(self.shared.admission.try_write().is_ok());
                assert!(self.connection.begin_posting().is_ok());
                self.observed.store(true, Ordering::Release);
            }
        }

        let Some((engine, driver)) = production_engine(2, 4, 4) else {
            return;
        };
        let shared = Arc::clone(&engine.shared);
        let poster = Arc::new(ScriptedPoster::new(&shared, 52, ScriptedPost::Unaccepted));
        let connection = scripted_connection(&shared, Arc::clone(&poster), 1, 1);
        let (io, events) =
            super::super::io::IoConnection::new(Arc::clone(&shared), Arc::clone(&connection.state))
                .unwrap();
        let wake = Arc::new(PostGuardCheckingWake {
            shared: Arc::clone(&shared),
            connection: Arc::clone(&connection.state),
            observed: AtomicBool::new(false),
        });
        events.register(&Waker::from(Arc::clone(&wake)));
        let posted = io.post_send(IoSendRequest::new(
            io.register_memory(64, AccessIntent::LocalOnly).unwrap(),
            1,
            IoOperationContext::new(()),
        ));
        assert!(matches!(
            posted,
            IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: 1,
                error: Error::PostFailed(_)
            }
        ));
        assert!(wake.observed.load(Ordering::Acquire));
        let Some(super::super::io::IoEvent::Completion(completion)) = events.pop() else {
            panic!("unaccepted event")
        };
        let (_, _, result, mr, unaccepted) = completion.into_parts();
        assert!(matches!(result, Err(Error::PostFailed(_))));
        assert!(mr.is_some());
        assert!(unaccepted);
        assert_eq!(shared.operations.live(), 0);
        assert_eq!(shared.accepted_operations.load(Ordering::Acquire), 0);
        assert_eq!(shared.cq_credits.free(), 4);
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

        shared.finish(MemoizedTerminalResult::from_error(Error::EngineWedged {
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
            "terminal wakes must run after admission and terminal guards drop"
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
            shared.dispatch_connection_completions(connection.state.token, 1),
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
        PrefixAccepted(usize),
        PrefixWithQueuedSuffixCompletion {
            accepted: usize,
            completed_suffix: usize,
        },
        PrefixWithSuffixCompletion {
            accepted: usize,
            completed_suffix: usize,
        },
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

        fn post(&self, tokens: Vec<OperationToken>, opcode: u32) -> BatchPostOutcome {
            self.calls.fetch_add(1, Ordering::AcqRel);
            lock_unpoison(&self.tokens).extend(tokens.iter().copied());
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
                    let token = tokens[0];
                    let shared = self.shared.upgrade().expect("engine shared state");
                    assert_eq!(
                        shared.enqueue_completion(wc(token, self.qp_num, opcode)),
                        shared.connections.lookup_qp(self.qp_num)
                    );
                    let connection = shared
                        .connections
                        .lookup_qp(self.qp_num)
                        .expect("connection token");
                    assert_eq!(
                        shared.dispatch_connection_completions(connection, 1),
                        (1, false)
                    );
                    BatchPostOutcome::AllAccepted
                }
                ScriptedPost::PrefixAccepted(accepted) => BatchPostOutcome::PrefixAccepted {
                    accepted,
                    first_unaccepted: accepted,
                    source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                },
                ScriptedPost::PrefixWithQueuedSuffixCompletion {
                    accepted,
                    completed_suffix,
                } => {
                    assert!(completed_suffix >= accepted);
                    let token = tokens[completed_suffix];
                    let shared = self.shared.upgrade().expect("engine shared state");
                    assert_eq!(
                        shared.enqueue_completion(wc(token, self.qp_num, opcode)),
                        shared.connections.lookup_qp(self.qp_num)
                    );
                    BatchPostOutcome::PrefixAccepted {
                        accepted,
                        first_unaccepted: accepted,
                        source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                    }
                }
                ScriptedPost::PrefixWithSuffixCompletion {
                    accepted,
                    completed_suffix,
                } => {
                    assert!(completed_suffix >= accepted);
                    let token = tokens[completed_suffix];
                    let shared = self.shared.upgrade().expect("engine shared state");
                    assert_eq!(
                        shared.enqueue_completion(wc(token, self.qp_num, opcode)),
                        shared.connections.lookup_qp(self.qp_num)
                    );
                    let connection = shared
                        .connections
                        .lookup_qp(self.qp_num)
                        .expect("connection token");
                    assert_eq!(
                        shared.dispatch_connection_completions(connection, 1),
                        (1, false)
                    );
                    BatchPostOutcome::PrefixAccepted {
                        accepted,
                        first_unaccepted: accepted,
                        source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                    }
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

        fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            Ok(self.post(
                (0..batch.len())
                    .map(|index| OperationToken::decode(batch.wr_id_for_test(index)))
                    .collect(),
                IBV_WC_SEND,
            ))
        }

        fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            Ok(self.post(
                (0..batch.len())
                    .map(|index| OperationToken::decode(batch.wr_id_for_test(index)))
                    .collect(),
                IBV_WC_RECV,
            ))
        }

        fn to_error(&self) -> Result<()> {
            self.error_transitions.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn destroy_qp(&self) -> Result<bool> {
            Ok(false)
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
            shared.dispatch_connection_completions(connection.token, 1),
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
            Ok(false)
        }
        #[cfg(any(test, feature = "test-hooks"))]
        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    fn synthetic_engine(capacity: usize) -> Arc<EngineShared> {
        let mut config = EngineConfig::new("test0".into());
        config.max_live_connections = capacity;
        config.max_inflight_operations = capacity;
        config.cq_capacity = capacity;
        Arc::new(EngineShared::new(config, None, None).unwrap())
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

    fn install_accepted_with_result(
        shared: &Arc<EngineShared>,
        connection: &Arc<ConnectionState>,
        opcode: WcOpcode,
    ) -> (OperationToken, super::super::io::IoEventReceiver) {
        let direction = match opcode {
            WcOpcode::Recv => Direction::Recv,
            _ => Direction::Send,
        };
        connection.reserve_local(direction).unwrap();
        assert!(shared.cq_credits.reserve());
        let (sender, events) = super::super::io::event_port();
        let (token, operation) = shared
            .operations
            .allocate(|token| {
                Arc::new(OperationState::new_with_event(
                    token,
                    Arc::clone(connection),
                    direction,
                    opcode,
                    None,
                    1,
                    Some(IoEventDestination::new(sender, IoOperationContext::new(()))),
                ))
            })
            .unwrap();
        operation.commit_accepted();
        shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
        (token, events)
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
