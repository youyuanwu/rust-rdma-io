//! Scalar operation future, bounded command admission, and cancellation.
//!
//! `RdmaOperation` is the only caller-held handle for a single SEND, RECV,
//! READ, or WRITE. Its first poll is the sole provider-post boundary for the
//! scalar path: the frontend first poll acquires bounded ingress and enqueues
//! owned input without calling the provider. A later `RdmaEngineDriver` poll
//! invokes `start_operation`, which validates, takes the admission and posting
//! guards, reserves the local direction, registry slot, and CQ credit, posts
//! once, and reconciles the outcome. No frontend poll or `Drop` calls the
//! provider.
//!
//! `FutureState` stays private so the pre-post, in-flight, and resolved stages
//! cannot be assembled or skipped from outside. That is why the sibling-test
//! fixture uses the `cfg(test)` [`RdmaOperation::from_in_flight`] constructor
//! instead of a struct literal.
//!
//! The module observes the same ownership rules as its `batch` sibling:
//!
//! - It reaches value-owned `OperationState` records only through the
//!   reactor-owned registry. Reservation, acceptance, proven non-acceptance,
//!   detachment, and cancellation never share a backend record.
//! - It never reads or builds effect payload fields. Post-lock work is carried
//!   in `AfterEngineUnlock` and published only by
//!   by the reactor action path after both guards are dropped.
//! - It releases provider-visible ownership only on positive proof. A rollback
//!   before registration returns the MR directly; a proven-unaccepted post
//!   returns it through `take_unaccepted`; anything ambiguous is committed as
//!   accepted and left to reclamation.
//!
//! `Drop` follows the same split: a future dropped before its first poll owns
//! an unregistered MR and simply frees it, while a dropped in-flight future
//! only cancels and schedules reclamation, retaining the MR, registration, and
//! CQ debt until an exact CQE or QP-destruction proof arrives.
//!
//! Submission validation lives in the sibling `validation` module, which is
//! shared with `batch`; neither submission path depends on the other.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLockReadGuard, Weak};
use std::task::{Context, Poll};

use tokio::sync::OwnedSemaphorePermit;

use crate::v2::engine::reactor::CommandIngress;
use crate::v2::engine::registry::lock_unpoison;
use crate::v2::engine::registry::{ConnectionToken, Lookup, OperationToken};
use crate::v2::engine::session::SessionManager;
use crate::v2::error::{Error, Result};
use crate::v2::mr::{Mr, RemoteMr};
use crate::v2::op::Completion;
use crate::v2::qp::BatchPostOutcome;
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr};

use super::super::{ConnectionIoState, EstablishedIoConnection, IoState, OperationKind};
use super::effects::AfterEngineUnlock;
use super::state::{OperationObserver, OperationState};
use super::validation::ValidatedOperation;

struct PostingTurnGuard;

impl Drop for PostingTurnGuard {
    fn drop(&mut self) {}
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
/// The first poll only performs bounded command admission. The explicit engine
/// driver later performs `ibv_post_send` or `ibv_post_recv`; those provider
/// calls have no wall-clock latency guarantee even though completion is
/// asynchronous.
pub struct RdmaOperation {
    state: FutureState,
}

enum FutureState {
    PreAdmission {
        commands: Weak<CommandIngress>,
        manager: Weak<SessionManager>,
        connection: ConnectionToken,
        kind: OperationKind,
        mr: Option<Mr>,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    },
    Waiting {
        commands: Arc<CommandIngress>,
        manager: Weak<SessionManager>,
        connection: ConnectionToken,
        permit: Pin<Box<dyn Future<Output = Option<OwnedSemaphorePermit>> + Send>>,
        kind: OperationKind,
        mr: Option<Mr>,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    },
    Queued {
        commands: Weak<CommandIngress>,
        command: Weak<OperationCommand>,
        completion: Arc<OperationCommandCompletion>,
    },
    Immediate(Option<(Result<Completion>, Option<Mr>)>),
    Done,
}

impl Unpin for RdmaOperation {}

impl RdmaOperation {
    pub(in crate::v2::engine) fn new(
        commands: Weak<CommandIngress>,
        manager: Weak<SessionManager>,
        connection: ConnectionToken,
        kind: OperationKind,
        mr: Mr,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
    ) -> Self {
        Self {
            state: FutureState::PreAdmission {
                commands,
                manager,
                connection,
                kind,
                mr: Some(mr),
                remote,
                range,
            },
        }
    }

    /// Builds an already-accepted in-flight future for sibling operation tests.
    ///
    /// Test fixtures need a future whose `Drop` exercises the in-flight
    /// cancellation path without a provider. Exposing this constructor keeps
    /// `state` and `FutureState` private instead of widening them for tests.
    #[cfg(test)]
    pub(super) fn from_in_flight(token: OperationToken, observer: Arc<OperationObserver>) -> Self {
        let completion = Arc::new(OperationCommandCompletion::from_observer(observer));
        completion.install(StartResult::InFlight(token));
        Self {
            state: FutureState::Queued {
                commands: Weak::new(),
                command: Weak::new(),
                completion,
            },
        }
    }
}

impl Future for RdmaOperation {
    type Output = (Result<Completion>, Option<Mr>);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match &mut self.state {
                FutureState::PreAdmission { .. } => {
                    let pending = std::mem::replace(&mut self.state, FutureState::Done);
                    let FutureState::PreAdmission {
                        commands,
                        manager,
                        connection,
                        kind,
                        mr,
                        remote,
                        range,
                    } = pending
                    else {
                        return Poll::Ready((Err(Error::DriverShutdown), None));
                    };
                    let Some(commands) = commands.upgrade() else {
                        self.state = FutureState::Immediate(Some((Err(Error::DriverShutdown), mr)));
                        continue;
                    };
                    let permit = Box::pin(commands.operation_acquire());
                    self.state = FutureState::Waiting {
                        commands,
                        manager,
                        connection,
                        permit,
                        kind,
                        mr,
                        remote,
                        range,
                    };
                }
                FutureState::Waiting { permit, .. } => {
                    let Poll::Ready(permit) = permit.as_mut().poll(cx) else {
                        return Poll::Pending;
                    };
                    let pending = std::mem::replace(&mut self.state, FutureState::Done);
                    let FutureState::Waiting {
                        commands,
                        manager,
                        connection,
                        kind,
                        mut mr,
                        remote,
                        range,
                        ..
                    } = pending
                    else {
                        unreachable!("operation admission state changed while polling");
                    };
                    let Some(permit) = permit else {
                        let error = manager
                            .upgrade()
                            .and_then(|manager| manager.admission_error())
                            .unwrap_or(Error::DriverShutdown);
                        self.state = FutureState::Immediate(Some((Err(error), mr)));
                        continue;
                    };
                    let Some(manager) = manager.upgrade() else {
                        self.state = FutureState::Immediate(Some((Err(Error::DriverShutdown), mr)));
                        continue;
                    };
                    let Some(mr) = mr.take() else {
                        self.state =
                            FutureState::Immediate(Some((Err(Error::DriverShutdown), None)));
                        continue;
                    };
                    let completion = Arc::new(OperationCommandCompletion::new());
                    completion.register(cx.waker());
                    let command = Arc::new(OperationCommand::new(
                        connection,
                        kind,
                        mr,
                        remote,
                        range,
                        Arc::clone(&completion),
                    ));
                    if let Err(error) =
                        commands.enqueue_operation(&manager, Arc::clone(&command), permit)
                    {
                        command.cancel_before_execution(error);
                    }
                    commands.publish_command_work();
                    cx.waker().wake_by_ref();
                    self.state = FutureState::Queued {
                        commands: Arc::downgrade(&commands),
                        command: Arc::downgrade(&command),
                        completion,
                    };
                    return Poll::Pending;
                }
                FutureState::Queued { completion, .. } => {
                    if let Poll::Ready(output) = completion.poll(cx) {
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
        if let FutureState::Queued {
            commands,
            command,
            completion,
        } = state
        {
            completion.cancel();
            if let (Some(commands), Some(command)) = (commands.upgrade(), command.upgrade())
                && !commands.cancel_operation(&command)
                && let Some(token) = completion.in_flight_token()
            {
                commands.request_operation_cancel(token);
            }
        }
    }
}

struct OperationInput {
    connection: ConnectionToken,
    kind: OperationKind,
    mr: Mr,
    remote: Option<RemoteMr>,
    range: Option<(usize, usize)>,
}

pub(in crate::v2::engine) struct OperationCommand {
    input: Mutex<Option<OperationInput>>,
    completion: Arc<OperationCommandCompletion>,
}

impl OperationCommand {
    fn new(
        connection: ConnectionToken,
        kind: OperationKind,
        mr: Mr,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
        completion: Arc<OperationCommandCompletion>,
    ) -> Self {
        Self {
            input: Mutex::new(Some(OperationInput {
                connection,
                kind,
                mr,
                remote,
                range,
            })),
            completion,
        }
    }

    pub(in crate::v2::engine) fn execute_into(
        &self,
        connections: &mut crate::v2::engine::session::registry::ConnectionRegistry,
        shared: &mut IoState,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let Some(input) = lock_unpoison(&self.input).take() else {
            return;
        };
        if self.completion.is_cancelled() {
            self.completion.install_into(
                StartResult::Immediate((Err(Error::DriverShutdown), Some(input.mr))),
                shared,
                actions,
            );
            return;
        }
        if !connections.io_is_open(input.connection) {
            self.completion.install_into(
                StartResult::Immediate((Err(Error::TransportClosed), Some(input.mr))),
                shared,
                actions,
            );
            return;
        }
        let result = connections
            .with_connection_io_mut(input.connection, |connection, connection_io, poster| {
                start_operation(
                    shared,
                    connection,
                    connection_io,
                    poster,
                    input.kind,
                    input.mr,
                    input.remote,
                    input.range,
                    Arc::clone(&self.completion.observer),
                    actions,
                )
            })
            .expect("open connection retains its I/O bundle for the command turn");
        self.completion.install_into(result, shared, actions);
    }

    pub(in crate::v2::engine) fn cancel_before_execution(&self, error: Error) {
        let Some(input) = lock_unpoison(&self.input).take() else {
            self.completion.cancel();
            return;
        };
        self.completion
            .install(StartResult::Immediate((Err(error), Some(input.mr))));
    }

    pub(in crate::v2::engine) fn cancel_before_execution_into(
        &self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let Some(input) = lock_unpoison(&self.input).take() else {
            self.completion.cancel();
            return;
        };
        self.completion
            .observer
            .complete((Err(error), Some(input.mr)));
        actions.push_operation_wake(Arc::clone(&self.completion.observer));
    }
}

struct OperationCommandCompletion {
    observer: Arc<OperationObserver>,
    in_flight: Mutex<Option<OperationToken>>,
}

impl OperationCommandCompletion {
    fn new() -> Self {
        Self {
            observer: OperationObserver::new(),
            in_flight: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn from_observer(observer: Arc<OperationObserver>) -> Self {
        Self {
            observer,
            in_flight: Mutex::new(None),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.observer.is_cancelled()
    }

    fn register(&self, waker: &std::task::Waker) {
        self.observer.register(waker);
    }

    fn install(&self, result: StartResult) {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        match result {
            StartResult::InFlight(token) => {
                *lock_unpoison(&self.in_flight) = Some(token);
            }
            StartResult::Immediate(output) => self.observer.complete(output),
        }
        let observer = Arc::clone(&self.observer);
        actions.push_operation(move || observer.wake());
        actions.publish();
    }

    fn install_into(
        &self,
        result: StartResult,
        shared: &mut IoState,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let in_flight = match result {
            StartResult::InFlight(token) => {
                *lock_unpoison(&self.in_flight) = Some(token);
                Some(token)
            }
            StartResult::Immediate(output) => {
                self.observer.complete(output);
                None
            }
        };
        if self.is_cancelled()
            && let Some(token) = in_flight
        {
            shared.cancel_operation(token);
        }
        actions.push_operation_wake(Arc::clone(&self.observer));
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<(Result<Completion>, Option<Mr>)> {
        self.observer.poll(cx)
    }

    fn cancel(&self) {
        self.observer.cancel();
    }

    fn in_flight_token(&self) -> Option<OperationToken> {
        *lock_unpoison(&self.in_flight)
    }
}

/// Outcome of the single scalar posting attempt.
///
/// `InFlight` means the operation is registered and owned by the engine until
/// an exact CQE or reclamation proof resolves it. `Immediate` means the caller
/// keeps the result and whatever MR ownership survived the rollback.
enum StartResult {
    InFlight(OperationToken),
    Immediate((Result<Completion>, Option<Mr>)),
}

/// Validates, reserves, posts once, and reconciles what the provider accepted.
///
/// Every early return before registration is a zero-call rollback that hands
/// the MR straight back. After registration, each rollback releases the
/// registry slot, CQ credit, and local direction it actually took, in the
/// reverse order they were acquired. The accepted, exact-zero-prefix,
/// proven-unaccepted, and ambiguous arms then assign one — and only one —
/// owner to those reservations.
#[allow(
    clippy::too_many_arguments,
    reason = "provider transaction inputs stay explicit"
)]
fn start_operation(
    shared: &mut IoState,
    connection: &Arc<EstablishedIoConnection>,
    connection_io: &mut ConnectionIoState,
    poster: &dyn super::super::IoPostAuthority,
    kind: OperationKind,
    mr: Mr,
    remote: Option<RemoteMr>,
    range: Option<(usize, usize)>,
    observer: Arc<OperationObserver>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> StartResult {
    let validated = match ValidatedOperation::new(kind, &mr, remote, range) {
        Ok(validated) => validated,
        Err(error) => return StartResult::Immediate((Err(error), Some(mr))),
    };
    let admission_owner = Arc::clone(&shared.admission);
    let admission = crate::v2::engine::registry::read_unpoison(&admission_owner);
    if let Some(error) = shared.admission_error() {
        return StartResult::Immediate((Err(error), Some(mr)));
    }
    #[cfg(any(test, feature = "test-hooks"))]
    shared.pause_operation_before_register();
    let posting = PostingTurnGuard;
    let direction = kind.direction();
    if let Err(error) = shared.reserve_local(connection, connection_io, direction) {
        return StartResult::Immediate((Err(error), Some(mr)));
    }

    let expected_opcode = validated.expected_opcode();
    let mr_len = mr.len();
    let mut mr = Some(mr);
    let token = match shared.operations.allocate(|token| {
        OperationState::new_scalar(
            token,
            Arc::clone(connection),
            direction,
            expected_opcode,
            mr.take(),
            mr_len,
            Arc::clone(&observer),
        )
    }) {
        Ok(token) => token,
        Err(error) => {
            shared.release_local(connection, connection_io, direction);
            return StartResult::Immediate((Err(error), mr));
        }
    };

    if !shared.cq_credits.reserve() {
        let mut state = shared
            .operations
            .release(token, false)
            .expect("unposted operation remains registered");
        shared.release_local(connection, connection_io, direction);
        return StartResult::Immediate((Err(Error::CapacityExhausted), state.take_mr()));
    }
    let outcome = match post_validated_operation(validated, poster, token) {
        Ok(outcome) => outcome,
        Err(error) => {
            let mut state = shared
                .operations
                .release(token, false)
                .expect("unposted operation remains registered");
            shared.cq_credits.release();
            shared.release_local(connection, connection_io, direction);
            return StartResult::Immediate((Err(error), state.take_mr()));
        }
    };
    match outcome {
        BatchPostOutcome::AllAccepted => {
            shared.accepted_operations += 1;
            shared.add_accepted(connection, connection_io, token);
            let committed = match shared.operations.lookup_mut(token) {
                Lookup::Occupied(state) => state.commit_accepted(),
                _ => unreachable!("accepted operation remains registered"),
            };
            if committed.cancellation_needs_reclamation {
                shared.pending_reclamations += 1;
                shared.schedule_reclamation(token);
            }
            shared.publish_cq_recheck();
            if let Some(completion) = committed.early {
                let after_unlock = shared.finish_early_completion(connection_io, token, completion);
                append_after_post_guards(posting, admission, after_unlock, actions);
            }
            StartResult::InFlight(token)
        }
        BatchPostOutcome::PrefixAccepted {
            accepted,
            first_unaccepted,
            source,
        } if accepted == 0 && first_unaccepted == 0 => {
            let error = Error::PostFailed(source);
            let can_release = matches!(
                shared.operations.lookup(token),
                Lookup::Occupied(state) if state.can_release_unaccepted()
            );
            if can_release {
                let mut state = shared
                    .operations
                    .release(token, false)
                    .expect("proven-unaccepted operation remains registered");
                let release = state
                    .take_unaccepted(error.clone())
                    .expect("proven-unaccepted operation has no completion");
                shared.cq_credits.release();
                shared.release_local(connection, connection_io, direction);
                debug_assert!(release.event.is_none());
                drop(release.event);
                StartResult::Immediate((Err(error), release.mr))
            } else {
                shared.accepted_operations += 1;
                shared.add_accepted(connection, connection_io, token);
                let committed = match shared.operations.lookup_mut(token) {
                    Lookup::Occupied(state) => state.commit_accepted(),
                    _ => unreachable!("retained operation remains registered"),
                };
                if committed.cancellation_needs_reclamation {
                    shared.pending_reclamations += 1;
                    shared.schedule_reclamation(token);
                }
                shared.publish_cq_recheck();
                if let Some(completion) = committed.early {
                    let after_unlock =
                        shared.finish_early_completion(connection_io, token, completion);
                    append_after_post_guards(posting, admission, after_unlock, actions);
                }
                StartResult::InFlight(token)
            }
        }
        BatchPostOutcome::PrefixAccepted { source, .. }
        | BatchPostOutcome::Ambiguous { source } => {
            shared.accepted_operations += 1;
            shared.add_accepted(connection, connection_io, token);
            let committed = match shared.operations.lookup_mut(token) {
                Lookup::Occupied(state) => state.commit_accepted(),
                _ => unreachable!("ambiguous operation remains registered"),
            };
            if committed.cancellation_needs_reclamation {
                shared.pending_reclamations += 1;
                shared.schedule_reclamation(token);
            }
            shared.publish_cq_recheck();
            if let Some(completion) = committed.early {
                let after_unlock = shared.finish_early_completion(connection_io, token, completion);
                append_after_post_guards(posting, admission, after_unlock, actions);
                StartResult::InFlight(token)
            } else {
                let newly_pending = match shared.operations.lookup_mut(token) {
                    Lookup::Occupied(state) => state.detach_with_post_error(),
                    _ => false,
                };
                if newly_pending {
                    shared.pending_reclamations += 1;
                    shared.schedule_reclamation(token);
                }
                StartResult::Immediate((Err(Error::PostFailed(source)), None))
            }
        }
    }
}

/// Encodes the validated operation as a one-entry RECV or SEND batch and posts.
///
/// The work request carries the operation token as its `wr_id`, which is what
/// later CQE routing matches against, and consumes `validated` by value so no
/// checked input can be reused after encoding.
fn post_validated_operation(
    validated: ValidatedOperation,
    poster: &dyn super::super::IoPostAuthority,
    token: OperationToken,
) -> Result<BatchPostOutcome> {
    match validated.kind() {
        OperationKind::Recv => {
            let mut batch =
                PreparedRecvBatch::new(vec![RecvWr::new(token.encode()).sg(validated.sge())])
                    .map_err(Error::from_v1)?;
            poster.post_recv(&mut batch)
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
            poster.post_send(&mut batch)
        }
    }
}

fn append_after_post_guards(
    posting: PostingTurnGuard,
    admission: RwLockReadGuard<'_, ()>,
    after_unlock: AfterEngineUnlock,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) {
    drop(posting);
    drop(admission);
    after_unlock.append_to(actions);
}
