//! Scalar operation future, first-poll submission, and cancellation.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLockReadGuard};
use std::task::{Context, Poll};

use crate::v2::engine::registry::OperationToken;
use crate::v2::error::{Error, Result};
use crate::v2::mr::{Mr, RemoteMr};
use crate::v2::op::Completion;
use crate::v2::qp::BatchPostOutcome;
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr};

use super::super::{EstablishedIoConnection, IoCore, OperationKind};
use super::effects::AfterEngineUnlock;
use super::state::OperationState;
use super::validation::ValidatedOperation;

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

    #[cfg(test)]
    pub(super) fn from_in_flight(shared: Arc<IoCore>, operation: Arc<OperationState>) -> Self {
        Self {
            state: FutureState::InFlight { shared, operation },
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

pub(super) fn publish_after_post_guards(
    posting: RwLockReadGuard<'_, ()>,
    admission: RwLockReadGuard<'_, ()>,
    after_unlock: AfterEngineUnlock,
) {
    drop(posting);
    drop(admission);
    after_unlock.publish();
}
