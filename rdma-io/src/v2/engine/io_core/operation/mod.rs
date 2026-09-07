//! Owned low-level operation futures, admission, and exact CQE routing.

mod accounting;
mod batch;
mod effects;
mod state;
mod validation;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLockReadGuard, Weak};
use std::task::{Context, Poll};
#[cfg(test)]
use std::{
    sync::Mutex,
    sync::atomic::{AtomicBool, AtomicUsize},
};

#[cfg(test)]
use super::super::io::{
    IoEventDestination, IoOperationContext, IoRecvRequest, IoSendRequest, IoSubmissionDisposition,
};
use super::super::lifecycle::MemoizedTerminalResult;
#[cfg(any(test, feature = "test-hooks"))]
use super::super::registry::lock_unpoison;
use super::super::registry::{ConnectionToken, Lookup, OperationToken};
#[cfg(test)]
use super::Direction;
use super::{EstablishedIoConnection, IoCore, OperationKind};
use crate::v2::error::{Error, Result};
use crate::v2::mr::{Mr, RemoteMr};
use crate::v2::op::Completion;
use crate::v2::qp::BatchPostOutcome;
#[cfg(test)]
use crate::wc::WcOpcode;
use crate::wc::WorkCompletion;
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr};
#[cfg(test)]
use crate::wr::{Sge, WrOpcode};

pub(super) use accounting::{CqCreditPool, OperationRegistry};
#[cfg(test)]
use batch::test_support::{
    InternalBatchEntry, InternalRelease, commit_internal_entries, release_proven_unaccepted_entries,
};
#[cfg(test)]
use batch::{BatchOwnershipTransfer, PreparedBatchOwnership};
pub(in crate::v2::engine) use batch::{post_io_recv_batch, post_io_send};
use effects::{AfterEngineUnlock, DetachedIoCoreEffects};
pub(in crate::v2::engine) use effects::{
    CommittedIoCoreEffects, IoCoreEffects, OperationQuarantineEffect,
};
#[cfg(test)]
use state::OperationLifecycle;
use state::{CompletionDisposition, OperationState};
use validation::ValidatedOperation;

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
                    operation.register_waker(cx.waker());
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
            shared.schedule_reclamation(operation.token());
        }
    }
}

enum StartResult {
    InFlight(Arc<OperationState>),
    Immediate((Result<Completion>, Option<Mr>)),
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
    let posting = match connection.begin_posting() {
        Ok(posting) => posting,
        Err(error) => return StartResult::Immediate((Err(error), Some(mr))),
    };
    let direction = kind.direction();
    if let Err(error) = connection.reserve_local(direction) {
        return StartResult::Immediate((Err(error), Some(mr)));
    }

    let expected_opcode = validated.expected_opcode();
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
    let outcome = match post_validated_operation(validated, connection, token) {
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
            if let Some(completion) = early {
                let after_unlock = shared.finish_early_completion(Arc::clone(&state), completion);
                publish_after_post_guards(posting, admission, after_unlock);
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
                if let Some(completion) = early {
                    let after_unlock =
                        shared.finish_early_completion(Arc::clone(&state), completion);
                    publish_after_post_guards(posting, admission, after_unlock);
                }
                StartResult::InFlight(state)
            }
        }
        BatchPostOutcome::PrefixAccepted { source, .. }
        | BatchPostOutcome::Ambiguous { source } => {
            shared.accepted_operations.fetch_add(1, Ordering::AcqRel);
            let early = state.commit_accepted();
            shared.publish_cq_recheck();
            if let Some(completion) = early {
                let after_unlock = shared.finish_early_completion(Arc::clone(&state), completion);
                publish_after_post_guards(posting, admission, after_unlock);
                StartResult::InFlight(state)
            } else {
                state.detach_with_post_error(shared);
                shared.schedule_reclamation(token);
                StartResult::Immediate((Err(Error::PostFailed(source)), None))
            }
        }
    }
}

fn post_validated_operation(
    validated: ValidatedOperation,
    connection: &EstablishedIoConnection,
    token: OperationToken,
) -> Result<BatchPostOutcome> {
    match validated.kind() {
        OperationKind::Recv => {
            let mut batch =
                PreparedRecvBatch::new(vec![RecvWr::new(token.encode()).sg(validated.sge())])
                    .map_err(Error::from_v1)?;
            connection.post_recv(&mut batch)
        }
        OperationKind::Send | OperationKind::Write | OperationKind::Read => {
            let opcode = validated.kind().send_wr_opcode().ok_or_else(|| {
                Error::InvalidConfig("RECV cannot be encoded as a SEND work request".into())
            })?;
            let mut wr = SendWr::new(token.encode(), opcode)
                .sg(validated.sge())
                .flags(SendFlags::SIGNALED);
            if let Some(remote) = validated.remote() {
                wr = wr.rdma(remote.addr, remote.rkey);
            }
            let mut batch = PreparedSendBatch::new(vec![wr]).map_err(Error::from_v1)?;
            connection.post_send(&mut batch)
        }
    }
}

fn publish_after_post_guards(
    posting: RwLockReadGuard<'_, ()>,
    admission: RwLockReadGuard<'_, ()>,
    after_unlock: AfterEngineUnlock,
) {
    drop(posting);
    drop(admission);
    after_unlock.publish();
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
        self.operation.connection().identity()
    }
}

impl IoCore {
    pub(in crate::v2::engine) fn fail_observers_for_close(
        &self,
        tokens: &[OperationToken],
        error: Error,
    ) -> DetachedIoCoreEffects {
        let mut after_unlock = AfterEngineUnlock::default();
        for token in tokens.iter().copied() {
            if let Lookup::Occupied(operation) = self.operations.lookup(token)
                && operation.fail_observer_for_close(error.clone())
            {
                after_unlock.push_operation_wake(operation);
            }
        }
        DetachedIoCoreEffects::new(after_unlock)
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
                    .fetch_add(operation.mr_len(), Ordering::AcqRel);
                self.cq_credits.retain();
                effects.push_quarantine(OperationQuarantineEffect::Added {
                    operation: operation.token(),
                    connection: operation.connection_token(),
                });
            }
            if terminalized.should_wake {
                effects.push_operation_wake(operation);
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
                    .fetch_add(operation.mr_len(), Ordering::AcqRel);
                self.cq_credits.retain();
                effects.push_quarantine(OperationQuarantineEffect::Added {
                    operation: operation.token(),
                    connection: operation.connection_token(),
                });
            }
            if terminalized.should_wake {
                effects.push_operation_wake(operation);
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
        let identity = pending.operation.connection().identity();
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
            && pending.completion.opcode() != pending.operation.expected_opcode()
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
        if self.operations.release(operation.token(), true).is_none() {
            self.reject_cqe(CqeReject::Duplicate);
            return IoCoreEffects::default();
        }
        let removed = operation.connection().remove_accepted(operation.token());
        operation.connection().release_local(operation.direction());
        self.cq_credits.release();
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        self.publish_io_if_drained(previous);
        let finished = operation.finish_completion(completion);
        if finished.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        let mut effects = IoCoreEffects::default();
        if let Some(event) = finished.event {
            effects.push_event(event);
        }
        effects.push_operation_wake(Arc::clone(&operation));
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_mrs.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_bytes
                .fetch_sub(operation.mr_len(), Ordering::AcqRel);
            effects.push_quarantine(OperationQuarantineEffect::Cleared {
                operation: operation.token(),
                connection: operation.connection_token(),
            });
        }
        if removed
            && !operation.connection().is_posting_open()
            && operation.connection().accepted_count() == 0
        {
            effects.push_drained(operation.connection_token());
        }
        effects
    }

    fn finish_early_completion(
        &self,
        operation: Arc<OperationState>,
        completion: WorkCompletion,
    ) -> AfterEngineUnlock {
        self.finish_operation(operation, completion)
            .into_after_unlock()
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
        connection.release_local(operation.direction());
        self.cq_credits.release();
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        self.publish_io_if_drained(previous);
        let finished = operation.finish_after_qp_destroy(close_error);
        if finished.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        let mut effects = IoCoreEffects::default();
        if let Some(event) = finished.event {
            effects.push_event(event);
        }
        effects.push_operation_wake(Arc::clone(&operation));
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_mrs.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_bytes
                .fetch_sub(operation.mr_len(), Ordering::AcqRel);
            effects.push_quarantine(OperationQuarantineEffect::Cleared {
                operation: operation.token(),
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
            .fetch_add(operation.mr_len(), Ordering::AcqRel);
        self.cq_credits.retain();
        let mut effects = IoCoreEffects::default();
        effects.push_quarantine(OperationQuarantineEffect::Added {
            operation: operation.token(),
            connection: operation.connection_token(),
        });
        effects
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
pub(in crate::v2::engine) fn register_operation_waker_for_test(
    io_core: &IoCore,
    token: OperationToken,
    waker: &std::task::Waker,
) {
    let Lookup::Occupied(operation) = io_core.operations.lookup(token) else {
        panic!("test operation must remain registered")
    };
    operation.register_waker(waker);
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
