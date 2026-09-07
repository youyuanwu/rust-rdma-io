//! Owned low-level operation futures, admission, and exact CQE routing.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::super::io::{
    IoEventDestination, IoEventSender, IoOperationContext, IoOperationIdentity, IoRecvRequest,
    IoSendRequest, IoSubmissionDisposition, PendingIoEvent,
};
use super::super::lifecycle::MemoizedTerminalResult;
use super::super::registry::{
    ConnectionToken, Lookup, OperationToken, PagedRegistry, lock_unpoison,
};
use super::{Direction, EstablishedIoConnection, IoCore, OperationKind};
use crate::v2::error::{Error, Result};
use crate::v2::mr::{Mr, RemoteMr};
use crate::v2::op::Completion;
use crate::v2::qp::BatchPostOutcome;
use crate::wc::{WcOpcode, WorkCompletion};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum CqeReject {
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

pub(in crate::v2::engine) fn post_io_recv_batch(
    shared: &IoCore,
    connection: &Arc<EstablishedIoConnection>,
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

pub(in crate::v2::engine) fn post_io_send(
    shared: &IoCore,
    connection: &Arc<EstablishedIoConnection>,
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
    shared: &IoCore,
    connection: &Arc<EstablishedIoConnection>,
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
    let admission = shared.admission();
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
        InternalPreparedBatch::Recv(batch) => connection.post_recv(batch),
        InternalPreparedBatch::Send(batch) => connection.post_send(batch),
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

fn commit_internal_entries(shared: &IoCore, entries: Vec<InternalBatchEntry>) -> AfterEngineUnlock {
    shared
        .accepted_operations
        .fetch_add(entries.len(), Ordering::AcqRel);
    let mut early = Vec::new();
    for entry in entries {
        if let Some(completion) = entry.state.commit_accepted() {
            early.push((entry.state, completion));
        }
    }
    shared.publish_cq_recheck();
    let mut after_unlock = AfterEngineUnlock::default();
    for (state, completion) in early {
        let effects = shared.finish_operation(state, completion);
        assert!(
            effects.quarantine.is_empty(),
            "post reconciliation cannot produce quarantine effects"
        );
        assert!(
            effects.drained.is_empty(),
            "post reconciliation cannot produce accepted-zero effects"
        );
        after_unlock.extend(effects.after_unlock);
    }
    after_unlock
}

fn rollback_internal_entries(
    shared: &IoCore,
    connection: &impl EstablishedIoRef,
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

trait EstablishedIoRef {
    fn established_io(&self) -> &EstablishedIoConnection;
}

impl EstablishedIoRef for EstablishedIoConnection {
    fn established_io(&self) -> &EstablishedIoConnection {
        self
    }
}

impl EstablishedIoRef for Arc<EstablishedIoConnection> {
    fn established_io(&self) -> &EstablishedIoConnection {
        self
    }
}

#[cfg(test)]
impl EstablishedIoRef for Arc<super::super::session::connection::ConnectionState> {
    fn established_io(&self) -> &EstablishedIoConnection {
        &self.io
    }
}

fn release_proven_unaccepted_entries(
    shared: &IoCore,
    connection: &impl EstablishedIoRef,
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
        connection.established_io().release_local(direction);
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
        shared: Arc<IoCore>,
        connection: Arc<EstablishedIoConnection>,
        kind: OperationKind,
        mr: Option<Mr>,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    },
    InFlight {
        shared: Arc<IoCore>,
        operation: Arc<OperationState>,
    },
    Immediate(Option<(Result<Completion>, Option<Mr>)>),
    Done,
}

impl Unpin for RdmaOperation {}

impl RdmaOperation {
    pub(in crate::v2::engine) fn new(
        shared: Arc<IoCore>,
        connection: Arc<EstablishedIoConnection>,
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

pub(in crate::v2::engine) struct OperationRegistry {
    slots: PagedRegistry<OperationToken, Arc<OperationState>>,
}

impl OperationRegistry {
    pub(in crate::v2::engine) fn new(capacity: usize) -> Result<Self> {
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

    pub(in crate::v2::engine) fn lookup(
        &self,
        token: OperationToken,
    ) -> Lookup<Arc<OperationState>> {
        self.slots.lookup_cloned(token)
    }

    fn release(&self, token: OperationToken, completed: bool) -> Option<Arc<OperationState>> {
        self.slots.release(token, completed)
    }

    pub(in crate::v2::engine) fn live(&self) -> usize {
        self.slots.live()
    }

    pub(in crate::v2::engine) fn occupied(&self) -> Vec<Arc<OperationState>> {
        self.slots.occupied_cloned()
    }

    fn scan_occupied(
        &self,
        start: usize,
        budget: usize,
    ) -> (Vec<Arc<OperationState>>, usize, bool, usize) {
        self.slots.scan_occupied_cloned(start, budget)
    }
}

pub(in crate::v2::engine) struct CqCreditPool {
    capacity: usize,
    used: AtomicUsize,
    retained: AtomicUsize,
}

impl CqCreditPool {
    pub(in crate::v2::engine) fn new(capacity: usize) -> Self {
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

    pub(in crate::v2::engine) fn retain(&self) {
        self.retained.fetch_add(1, Ordering::AcqRel);
    }

    fn release_retained(&self) {
        let previous = self.retained.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "retained CQ credit must exist");
    }

    pub(in crate::v2::engine) fn free(&self) -> usize {
        self.capacity
            .saturating_sub(self.used.load(Ordering::Acquire))
    }

    pub(in crate::v2::engine) fn retained(&self) -> usize {
        self.retained.load(Ordering::Acquire)
    }
}

pub(in crate::v2::engine) struct OperationState {
    token: OperationToken,
    connection: Arc<EstablishedIoConnection>,
    direction: Direction,
    expected_opcode: WcOpcode,
    pub(in crate::v2::engine) mr_len: usize,
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

trait IntoEstablishedIoConnection {
    fn into_established_io(self) -> Arc<EstablishedIoConnection>;
}

impl IntoEstablishedIoConnection for Arc<EstablishedIoConnection> {
    fn into_established_io(self) -> Arc<EstablishedIoConnection> {
        self
    }
}

#[cfg(test)]
impl IntoEstablishedIoConnection for Arc<super::super::session::connection::ConnectionState> {
    fn into_established_io(self) -> Arc<EstablishedIoConnection> {
        Arc::clone(&self.io)
    }
}

impl OperationState {
    pub(in crate::v2::engine) fn token(&self) -> OperationToken {
        self.token
    }

    pub(in crate::v2::engine) fn connection_token(&self) -> ConnectionToken {
        self.connection.identity().connection
    }

    fn new(
        token: OperationToken,
        connection: impl IntoEstablishedIoConnection,
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
        connection: impl IntoEstablishedIoConnection,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
        event_destination: Option<IoEventDestination>,
    ) -> Self {
        let detached = event_destination.is_some();
        let connection = connection.into_established_io();
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

    fn cancel(&self, shared: &IoCore) -> bool {
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

    pub(in crate::v2::engine) fn mark_quarantined(&self) -> QuarantineTransition {
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

    pub(in crate::v2::engine) fn fail_observer_for_close(&self, error: Error) -> bool {
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

    fn detach_with_post_error(&self, shared: &IoCore) {
        let mut inner = lock_unpoison(&self.inner);
        inner.detached = true;
        inner.lifecycle = OperationLifecycle::Cancelled;
        shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
        inner.reclamation_pending = true;
        self.cancelled.store(true, Ordering::Release);
    }

    pub(in crate::v2::engine) fn finalize_terminal(
        &self,
        outcome: &MemoizedTerminalResult,
    ) -> TerminalizeState {
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

    pub(in crate::v2::engine) fn wake(&self) {
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

pub(in crate::v2::engine) struct QuarantineTransition {
    pub(in crate::v2::engine) newly_quarantined: bool,
    pub(in crate::v2::engine) was_reclaiming: bool,
}

pub(in crate::v2::engine) struct TerminalizeState {
    pub(in crate::v2::engine) was_reclaiming: bool,
    pub(in crate::v2::engine) newly_quarantined: bool,
    pub(in crate::v2::engine) should_wake: bool,
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
    shared: &IoCore,
    connection: &Arc<EstablishedIoConnection>,
    kind: OperationKind,
    mr: Mr,
    remote: Option<RemoteMr>,
    range: Option<(usize, usize)>,
) -> StartResult {
    let validated = match ValidatedOperation::new(kind, &mr, remote, range) {
        Ok(validated) => validated,
        Err(error) => return StartResult::Immediate((Err(error), Some(mr))),
    };
    let admission = shared.admission();
    if let Some(error) = shared.admission_error() {
        return StartResult::Immediate((Err(error), Some(mr)));
    }
    #[cfg(any(test, feature = "test-hooks"))]
    shared.pause_operation_before_register();
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
            shared.publish_cq_recheck();
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
                shared.publish_cq_recheck();
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
            shared.publish_cq_recheck();
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
        let expected_opcode = kind.expected_completion_opcode();
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

    fn post(
        self,
        connection: &EstablishedIoConnection,
        token: OperationToken,
    ) -> Result<BatchPostOutcome> {
        match self.kind {
            OperationKind::Recv => {
                let mut batch =
                    PreparedRecvBatch::new(vec![RecvWr::new(token.encode()).sg(self.sge)])
                        .map_err(Error::from_v1)?;
                connection.post_recv(&mut batch)
            }
            OperationKind::Send | OperationKind::Write | OperationKind::Read => {
                let opcode = self.kind.send_wr_opcode().ok_or_else(|| {
                    Error::InvalidConfig("RECV cannot be encoded as a SEND work request".into())
                })?;
                let mut wr = SendWr::new(token.encode(), opcode)
                    .sg(self.sge)
                    .flags(SendFlags::SIGNALED);
                if let Some(remote) = self.remote {
                    wr = wr.rdma(remote.addr, remote.rkey);
                }
                let mut batch = PreparedSendBatch::new(vec![wr]).map_err(Error::from_v1)?;
                connection.post_send(&mut batch)
            }
        }
    }
}

impl OperationKind {
    const fn expected_completion_opcode(self) -> WcOpcode {
        match self {
            Self::Send => WcOpcode::Send,
            Self::Recv => WcOpcode::Recv,
            Self::Write => WcOpcode::RdmaWrite,
            Self::Read => WcOpcode::RdmaRead,
        }
    }

    const fn send_wr_opcode(self) -> Option<WrOpcode> {
        match self {
            Self::Send => Some(WrOpcode::Send),
            Self::Write => Some(WrOpcode::RdmaWrite),
            Self::Read => Some(WrOpcode::RdmaRead),
            Self::Recv => None,
        }
    }
}

pub(in crate::v2::engine) struct PendingCompletion {
    completion: WorkCompletion,
    operation: Arc<OperationState>,
}

/// Non-forgeable port for proof-gated reclamation.
///
/// `IoCore::new` creates exactly one value and transfers it to the session
/// owner. The core has no dependency on the session proof or resource types.
pub(in crate::v2::engine) struct QpReclaimCapability {
    core: Weak<IoCore>,
}

impl QpReclaimCapability {
    pub(super) fn new(core: &Arc<IoCore>) -> Self {
        Self {
            core: Arc::downgrade(core),
        }
    }

    pub(in crate::v2::engine) fn reclaim(
        &self,
        destroyed_connection: ConnectionToken,
        destroyed_qp_num: u32,
        connection: &EstablishedIoConnection,
        close_error: Error,
        token: OperationToken,
    ) -> (bool, IoCoreEffects) {
        let Some(core) = self.core.upgrade() else {
            return (false, IoCoreEffects::default());
        };
        core.reclaim_after_qp_destroy(
            destroyed_connection,
            destroyed_qp_num,
            connection,
            close_error,
            token,
        )
    }
}

impl PendingCompletion {
    pub(in crate::v2::engine) fn identity(&self) -> super::EstablishedIoIdentity {
        self.operation.connection.identity()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum OperationQuarantineEffect {
    Added {
        operation: OperationToken,
        connection: ConnectionToken,
    },
    Cleared {
        operation: OperationToken,
        connection: ConnectionToken,
    },
}

#[derive(Default)]
pub(in crate::v2::engine) struct IoCoreEffects {
    after_unlock: AfterEngineUnlock,
    quarantine: Vec<OperationQuarantineEffect>,
    drained: Vec<ConnectionToken>,
}

impl IoCoreEffects {
    fn extend(&mut self, mut other: Self) {
        self.after_unlock.extend(other.after_unlock);
        self.quarantine.append(&mut other.quarantine);
        self.drained.append(&mut other.drained);
    }

    pub(in crate::v2::engine) fn take_quarantine(&mut self) -> Vec<OperationQuarantineEffect> {
        std::mem::take(&mut self.quarantine)
    }

    pub(in crate::v2::engine) fn take_drained(&mut self) -> Vec<ConnectionToken> {
        std::mem::take(&mut self.drained)
    }

    pub(in crate::v2::engine) fn publish(self) {
        assert!(
            self.quarantine.is_empty(),
            "engine must apply operation quarantine effects before publication"
        );
        assert!(
            self.drained.is_empty(),
            "engine must apply accepted-zero effects before publication"
        );
        self.after_unlock.publish();
    }
}

impl IoCore {
    pub(in crate::v2::engine) fn fail_observers_for_close(
        &self,
        tokens: &[OperationToken],
        error: Error,
    ) -> IoCoreEffects {
        let mut effects = IoCoreEffects::default();
        for token in tokens.iter().copied() {
            if let Lookup::Occupied(operation) = self.operations.lookup(token)
                && operation.fail_observer_for_close(error.clone())
            {
                effects.after_unlock.operations_to_wake.push(operation);
            }
        }
        effects
    }

    pub(in crate::v2::engine) fn terminalize_operations(
        &self,
        outcome: &MemoizedTerminalResult,
    ) -> IoCoreEffects {
        let mut effects = IoCoreEffects::default();
        if !outcome.is_error() {
            return effects;
        }
        for operation in self.operations.occupied() {
            let terminalized = operation.finalize_terminal(outcome);
            debug_assert!(
                !terminalized.was_reclaiming || terminalized.newly_quarantined,
                "terminal reclamation must transfer its retained MR and CQ debt to quarantine"
            );
            if terminalized.was_reclaiming {
                self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
            }
            if terminalized.newly_quarantined {
                self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
                self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
                self.quarantined_bytes
                    .fetch_add(operation.mr_len, Ordering::AcqRel);
                self.cq_credits.retain();
                effects.quarantine.push(OperationQuarantineEffect::Added {
                    operation: operation.token(),
                    connection: operation.connection_token(),
                });
            }
            if terminalized.should_wake {
                effects.after_unlock.operations_to_wake.push(operation);
            }
        }
        effects
    }

    pub(in crate::v2::engine) fn terminalize_operations_bounded(
        &self,
        outcome: &MemoizedTerminalResult,
        cursor: usize,
        budget: usize,
    ) -> (IoCoreEffects, usize, bool, usize) {
        let mut effects = IoCoreEffects::default();
        if !outcome.is_error() {
            return (effects, cursor, true, 0);
        }
        let (operations, next, complete, scanned) = self.operations.scan_occupied(cursor, budget);
        for operation in operations {
            let terminalized = operation.finalize_terminal(outcome);
            debug_assert!(
                !terminalized.was_reclaiming || terminalized.newly_quarantined,
                "terminal reclamation must transfer its retained MR and CQ debt to quarantine"
            );
            if terminalized.was_reclaiming {
                self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
            }
            if terminalized.newly_quarantined {
                self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
                self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
                self.quarantined_bytes
                    .fetch_add(operation.mr_len, Ordering::AcqRel);
                self.cq_credits.retain();
                effects.quarantine.push(OperationQuarantineEffect::Added {
                    operation: operation.token(),
                    connection: operation.connection_token(),
                });
            }
            if terminalized.should_wake {
                effects.after_unlock.operations_to_wake.push(operation);
            }
        }
        (effects, next, complete, scanned)
    }

    pub(in crate::v2::engine) fn reject_cqe(&self, reason: CqeReject) {
        #[cfg(any(test, feature = "test-hooks"))]
        {
            self.rejected_cqes.fetch_add(1, Ordering::Relaxed);
            lock_unpoison(&self.rejected_cqe_reasons).push(reason);
        }
        #[cfg(not(any(test, feature = "test-hooks")))]
        let _ = reason;
    }

    pub(in crate::v2::engine) fn prepare_completion(
        &self,
        completion: WorkCompletion,
    ) -> Option<PendingCompletion> {
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
        Some(PendingCompletion {
            completion,
            operation,
        })
    }

    pub(in crate::v2::engine) fn enqueue_prepared_completion(
        &self,
        pending: PendingCompletion,
        live: Option<super::super::registry::LiveIoConnectionProof>,
        connection: &Arc<EstablishedIoConnection>,
    ) -> Option<ConnectionToken> {
        let identity = pending.operation.connection.identity();
        if pending.completion.qp_num() != identity.qp_num {
            self.reject_cqe(CqeReject::WrongQpNum);
            return None;
        }
        if !live.is_some_and(|proof| proof.proves(identity.connection, identity.qp_num))
            || connection.identity() != identity
        {
            self.reject_cqe(CqeReject::WrongConnection);
            return None;
        }
        if pending.completion.is_success()
            && pending.completion.opcode() != pending.operation.expected_opcode
        {
            self.reject_cqe(CqeReject::UnexpectedOpcode);
            return None;
        }
        if !pending.operation.mark_completion_queued() {
            self.reject_cqe(CqeReject::Duplicate);
            return None;
        }
        connection.enqueue_completion(pending.completion);
        Some(identity.connection)
    }

    pub(in crate::v2::engine) fn dispatch_connection_completions(
        &self,
        connection: &EstablishedIoConnection,
        quantum: usize,
    ) -> (usize, bool, IoCoreEffects) {
        let mut processed = 0;
        let mut effects = IoCoreEffects::default();
        while processed < quantum {
            let Some(completion) = connection.pop_completion() else {
                break;
            };
            effects.extend(self.dispatch_connection_completion(completion));
            processed += 1;
        }
        (processed, connection.has_completion_work(), effects)
    }

    fn dispatch_queued_completion(&self, completion: WorkCompletion) -> IoCoreEffects {
        let token = OperationToken::decode(completion.wr_id());
        let operation = match self.operations.lookup(token) {
            Lookup::Occupied(operation) => operation,
            Lookup::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                return IoCoreEffects::default();
            }
            Lookup::Stale => {
                self.reject_cqe(CqeReject::StaleOperation);
                return IoCoreEffects::default();
            }
            Lookup::Retired => {
                self.reject_cqe(CqeReject::RetiredOperation);
                return IoCoreEffects::default();
            }
            Lookup::Unknown => {
                self.reject_cqe(CqeReject::Unknown);
                return IoCoreEffects::default();
            }
        };
        match operation.record_completion(completion) {
            CompletionDisposition::Deferred => IoCoreEffects::default(),
            CompletionDisposition::Complete => self.finish_operation(operation, completion),
            CompletionDisposition::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                IoCoreEffects::default()
            }
        }
    }

    fn finish_operation(
        &self,
        operation: Arc<OperationState>,
        completion: WorkCompletion,
    ) -> IoCoreEffects {
        if self.operations.release(operation.token, true).is_none() {
            self.reject_cqe(CqeReject::Duplicate);
            return IoCoreEffects::default();
        }
        let removed = operation.connection.remove_accepted(operation.token);
        operation.connection.release_local(operation.direction);
        self.cq_credits.release();
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        self.publish_io_if_drained(previous);
        let finished = operation.finish_completion(completion);
        if finished.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        let mut effects = IoCoreEffects {
            after_unlock: AfterEngineUnlock {
                events: finished.event.into_iter().collect(),
                operations_to_wake: vec![Arc::clone(&operation)],
            },
            ..IoCoreEffects::default()
        };
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_mrs.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_bytes
                .fetch_sub(operation.mr_len, Ordering::AcqRel);
            effects.quarantine.push(OperationQuarantineEffect::Cleared {
                operation: operation.token,
                connection: operation.connection_token(),
            });
        }
        if removed
            && !operation.connection.is_posting_open()
            && operation.connection.accepted_count() == 0
        {
            effects.drained.push(operation.connection_token());
        }
        effects
    }

    fn reclaim_after_qp_destroy(
        &self,
        destroyed_connection: ConnectionToken,
        destroyed_qp_num: u32,
        connection: &EstablishedIoConnection,
        close_error: Error,
        token: OperationToken,
    ) -> (bool, IoCoreEffects) {
        let identity = connection.identity();
        if destroyed_connection != identity.connection || destroyed_qp_num != identity.qp_num {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "operation reclaim rejected a mismatched QP destruction proof"
            );
            return (false, IoCoreEffects::default());
        }
        let Lookup::Occupied(operation) = self.operations.lookup(token) else {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction left an accepted token without an operation registration"
            );
            return (false, IoCoreEffects::default());
        };
        if operation.connection_token() != identity.connection {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                owner = operation.connection_token().encode(),
                "QP destruction found an accepted token owned by another connection"
            );
            return (false, IoCoreEffects::default());
        }
        if !connection.remove_accepted(token) {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction reclaim lost accepted-set membership"
            );
            return (false, IoCoreEffects::default());
        }
        if self.operations.release(token, false).is_none() {
            connection.add_accepted(token);
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction reclaim could not retire the operation registration"
            );
            return (false, IoCoreEffects::default());
        }
        connection.release_local(operation.direction);
        self.cq_credits.release();
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        self.publish_io_if_drained(previous);
        let finished = operation.finish_after_qp_destroy(close_error);
        if finished.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        let mut effects = IoCoreEffects {
            after_unlock: AfterEngineUnlock {
                events: finished.event.into_iter().collect(),
                operations_to_wake: vec![Arc::clone(&operation)],
            },
            ..IoCoreEffects::default()
        };
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_mrs.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_bytes
                .fetch_sub(operation.mr_len, Ordering::AcqRel);
            effects.quarantine.push(OperationQuarantineEffect::Cleared {
                operation: operation.token,
                connection: operation.connection_token(),
            });
        }
        (true, effects)
    }

    fn dispatch_connection_completion(&self, completion: WorkCompletion) -> IoCoreEffects {
        // CQ routing and terminal publication share the admission barrier.
        // The guard covers one completion and drops before the caller applies
        // session effects or publishes events and operation wakes.
        let _admission = self.admission();
        self.dispatch_queued_completion(completion)
    }

    fn dispatch_queued_completions(
        &self,
        connection: &EstablishedIoConnection,
        budget: usize,
    ) -> (bool, IoCoreEffects) {
        let mut effects = IoCoreEffects::default();
        for _ in 0..budget {
            let Some(completion) = connection.pop_completion() else {
                return (false, effects);
            };
            effects.extend(self.dispatch_connection_completion(completion));
        }
        (connection.has_completion_work(), effects)
    }

    pub(in crate::v2::engine) fn reject_queued_completions_after_qp_destroy(
        &self,
        connection: &EstablishedIoConnection,
    ) -> (bool, IoCoreEffects) {
        self.dispatch_queued_completions(connection, self.completion_dispatch_budget)
    }

    pub(in crate::v2::engine) fn begin_reclamation(&self, token: OperationToken) {
        if let Lookup::Occupied(operation) = self.operations.lookup(token) {
            operation.mark_reclaiming();
        }
    }

    pub(in crate::v2::engine) fn handle_reclamation_deadline(
        &self,
        token: OperationToken,
    ) -> IoCoreEffects {
        self.quarantine_operation(token)
    }

    pub(in crate::v2::engine) fn quarantine_operation(
        &self,
        token: OperationToken,
    ) -> IoCoreEffects {
        let Lookup::Occupied(operation) = self.operations.lookup(token) else {
            return IoCoreEffects::default();
        };
        let transition = operation.mark_quarantined();
        if !transition.newly_quarantined {
            return IoCoreEffects::default();
        }
        if transition.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
        self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
        self.quarantined_bytes
            .fetch_add(operation.mr_len, Ordering::AcqRel);
        self.cq_credits.retain();
        IoCoreEffects {
            quarantine: vec![OperationQuarantineEffect::Added {
                operation: operation.token(),
                connection: operation.connection_token(),
            }],
            ..IoCoreEffects::default()
        }
    }
}

#[cfg(test)]
pub(in crate::v2::engine) fn install_accepted_operation_for_driver_test(
    io_core: &IoCore,
    connection: &Arc<super::super::session::connection::ConnectionState>,
    opcode: WcOpcode,
) -> OperationToken {
    let direction = if opcode == WcOpcode::Recv {
        Direction::Recv
    } else {
        Direction::Send
    };
    connection.reserve_local(direction).unwrap();
    assert!(io_core.cq_credits.reserve());
    let (token, operation) = io_core
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
    io_core.accepted_operations.fetch_add(1, Ordering::AcqRel);
    token
}

#[cfg(test)]
pub(in crate::v2::engine) fn operation_future_for_io_lifetime_test(
    io_core: &Arc<IoCore>,
    connection: &Arc<EstablishedIoConnection>,
) -> RdmaOperation {
    connection.reserve_local(Direction::Send).unwrap();
    assert!(io_core.cq_credits.reserve());
    let (_, operation) = io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(connection),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            ))
        })
        .unwrap();
    operation.commit_accepted();
    io_core.accepted_operations.fetch_add(1, Ordering::AcqRel);
    RdmaOperation {
        state: FutureState::InFlight {
            shared: Arc::clone(io_core),
            operation,
        },
    }
}

#[cfg(test)]
pub(in crate::v2::engine) fn completion_for_driver_test(
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
mod tests;
