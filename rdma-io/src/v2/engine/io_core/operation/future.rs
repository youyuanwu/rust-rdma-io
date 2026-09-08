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
//! - It never sees `OperationInner` or a state guard. Reservation, acceptance,
//!   proven non-acceptance, detachment, and cancellation are `OperationState`
//!   methods that return owned records.
//! - It never reads or builds effect payload fields. Post-lock work is carried
//!   in `AfterEngineUnlock` and published only by
//!   [`publish_after_post_guards`], after both guards are dropped.
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
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLockReadGuard, Weak};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;
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
        shared: Weak<IoCore>,
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
        shared: Weak<IoCore>,
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
        shared: Weak<IoCore>,
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
                shared,
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
    pub(super) fn from_in_flight(shared: Arc<IoCore>, operation: Arc<OperationState>) -> Self {
        let completion = Arc::new(OperationCommandCompletion::new());
        completion.install(StartResult::InFlight(operation), Arc::downgrade(&shared));
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
                        shared,
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
                        shared,
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
                        shared,
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
                        let error = shared
                            .upgrade()
                            .and_then(|shared| shared.admission_error())
                            .unwrap_or(Error::DriverShutdown);
                        self.state = FutureState::Immediate(Some((Err(error), mr)));
                        continue;
                    };
                    let Some(shared) = shared.upgrade() else {
                        self.state = FutureState::Immediate(Some((Err(Error::DriverShutdown), mr)));
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
                        Arc::downgrade(&shared),
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
            if let (Some(commands), Some(command)) = (commands.upgrade(), command.upgrade()) {
                commands.cancel_operation(&command);
            }
        }
    }
}

struct OperationInput {
    shared: Weak<IoCore>,
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
        shared: Weak<IoCore>,
        connection: ConnectionToken,
        kind: OperationKind,
        mr: Mr,
        remote: Option<RemoteMr>,
        range: Option<(usize, usize)>,
        completion: Arc<OperationCommandCompletion>,
    ) -> Self {
        Self {
            input: Mutex::new(Some(OperationInput {
                shared,
                connection,
                kind,
                mr,
                remote,
                range,
            })),
            completion,
        }
    }

    pub(in crate::v2::engine) fn execute(&self, manager: &SessionManager) {
        let Some(input) = lock_unpoison(&self.input).take() else {
            return;
        };
        let Some(shared) = input.shared.upgrade() else {
            self.completion.install(
                StartResult::Immediate((Err(Error::DriverShutdown), Some(input.mr))),
                Weak::new(),
            );
            return;
        };
        let connection = match manager.connections.lookup(input.connection) {
            Lookup::Occupied(connection) => Arc::clone(&connection.io),
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
                self.completion.install(
                    StartResult::Immediate((Err(Error::TransportClosed), Some(input.mr))),
                    Arc::downgrade(&shared),
                );
                return;
            }
        };
        if self.completion.is_cancelled() {
            self.completion.install(
                StartResult::Immediate((Err(Error::DriverShutdown), Some(input.mr))),
                Arc::downgrade(&shared),
            );
            return;
        }
        let result = start_operation(
            &shared,
            &connection,
            input.kind,
            input.mr,
            input.remote,
            input.range,
        );
        self.completion.install(result, Arc::downgrade(&shared));
    }

    pub(in crate::v2::engine) fn cancel_before_execution(&self, error: Error) {
        let Some(input) = lock_unpoison(&self.input).take() else {
            self.completion.cancel();
            return;
        };
        self.completion.install(
            StartResult::Immediate((Err(error), Some(input.mr))),
            input.shared,
        );
    }
}

enum OperationCommandResult {
    Pending,
    InFlight {
        shared: Weak<IoCore>,
        operation: Arc<OperationState>,
    },
    Immediate(Option<(Result<Completion>, Option<Mr>)>),
    Taken,
}

struct OperationCommandCompletion {
    state: Mutex<OperationCommandResult>,
    cancelled: std::sync::atomic::AtomicBool,
    waker: AtomicWaker,
}

impl OperationCommandCompletion {
    fn new() -> Self {
        Self {
            state: Mutex::new(OperationCommandResult::Pending),
            cancelled: std::sync::atomic::AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn register(&self, waker: &std::task::Waker) {
        self.waker.register(waker);
    }

    fn install(&self, result: StartResult, shared: Weak<IoCore>) {
        let mut state = lock_unpoison(&self.state);
        *state = match result {
            StartResult::InFlight(operation) => {
                OperationCommandResult::InFlight { shared, operation }
            }
            StartResult::Immediate(output) => OperationCommandResult::Immediate(Some(output)),
        };
        let cancelled = self.is_cancelled();
        let in_flight = match &*state {
            OperationCommandResult::InFlight { shared, operation } if cancelled => {
                Some((shared.clone(), Arc::clone(operation)))
            }
            _ => None,
        };
        drop(state);
        if let Some((shared, operation)) = in_flight
            && let Some(shared) = shared.upgrade()
            && operation.cancel(&shared)
        {
            shared.schedule_reclamation(operation.token());
        }
        self.waker.wake();
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<(Result<Completion>, Option<Mr>)> {
        loop {
            let in_flight = {
                let mut state = lock_unpoison(&self.state);
                match &mut *state {
                    OperationCommandResult::Pending => {
                        self.waker.register(cx.waker());
                        return Poll::Pending;
                    }
                    OperationCommandResult::Immediate(output) => {
                        let output = output
                            .take()
                            .unwrap_or_else(|| (Err(Error::DriverShutdown), None));
                        *state = OperationCommandResult::Taken;
                        return Poll::Ready(output);
                    }
                    OperationCommandResult::InFlight { operation, .. } => Arc::clone(operation),
                    OperationCommandResult::Taken => {
                        return Poll::Ready((Err(Error::DriverShutdown), None));
                    }
                }
            };
            in_flight.register_waker(cx.waker());
            if let Some(output) = in_flight.take_output() {
                *lock_unpoison(&self.state) = OperationCommandResult::Taken;
                return Poll::Ready(output);
            }
            return Poll::Pending;
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let in_flight = {
            let mut state = lock_unpoison(&self.state);
            match &mut *state {
                OperationCommandResult::InFlight { shared, operation } => {
                    Some((shared.clone(), Arc::clone(operation)))
                }
                OperationCommandResult::Immediate(output) => {
                    drop(output.take());
                    *state = OperationCommandResult::Taken;
                    None
                }
                OperationCommandResult::Pending | OperationCommandResult::Taken => None,
            }
        };
        if let Some((shared, operation)) = in_flight
            && let Some(shared) = shared.upgrade()
            && operation.cancel(&shared)
        {
            shared.schedule_reclamation(operation.token());
        }
    }
}

/// Outcome of the single scalar posting attempt.
///
/// `InFlight` means the operation is registered and owned by the engine until
/// an exact CQE or reclamation proof resolves it. `Immediate` means the caller
/// keeps the result and whatever MR ownership survived the rollback.
enum StartResult {
    InFlight(Arc<OperationState>),
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

/// Encodes the validated operation as a one-entry RECV or SEND batch and posts.
///
/// The work request carries the operation token as its `wr_id`, which is what
/// later CQE routing matches against, and consumes `validated` by value so no
/// checked input can be reused after encoding.
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

/// Drops both posting guards by value, then publishes the accumulated effects.
///
/// Taking the guards by value makes the ordering unforgeable: a caller cannot
/// publish an early-completion event or waker while still holding the posting
/// or admission lock. The `pub(super)` scope exists only so the parent module's
/// `cfg(test)` ordering test can call this boundary directly; no production
/// code outside this module uses it.
pub(super) fn publish_after_post_guards(
    posting: RwLockReadGuard<'_, ()>,
    admission: RwLockReadGuard<'_, ()>,
    after_unlock: AfterEngineUnlock,
) {
    drop(posting);
    drop(admission);
    after_unlock.publish();
}
