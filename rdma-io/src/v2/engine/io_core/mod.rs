//! Low-level operation/completion state composed by the v2 engine.

mod operation;
mod progress;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use super::io::{IoEventSender, IoTerminalEvent, PendingIoEvent};
use super::registry::{ConnectionToken, Lookup, OperationToken, lock_unpoison};
use crate::v2::error::{Error, Result};
use crate::v2::qp::BatchPostOutcome;
use crate::wc::WorkCompletion;
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};
pub(super) use operation::CqeReject;
pub use operation::RdmaOperation;
pub(in crate::v2::engine) use operation::future::OperationCommand;
pub(super) use operation::{
    CommittedIoCoreEffects, IoCoreEffects, OperationObserver, OperationQuarantineEffect,
    post_io_recv_batch, post_io_recv_batch_into, post_io_send_into,
};
use operation::{CqCreditPool, OperationRegistry};
#[cfg(test)]
pub(super) use operation::{
    completion_for_driver_test, install_accepted_operation_for_driver_test,
    operation_future_for_io_lifetime_test, register_operation_waker_for_test,
};
pub(super) use progress::IoReactorSources;

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
    #[cfg(any(test, feature = "test-hooks"))]
    fn pause_operation_before_register(&self);
}

/// Narrow session capability needed by owner-local I/O progress.
///
/// The I/O side never receives a concrete session manager, registry, lifecycle
/// authority, or resource bundle through this boundary.
#[cfg(test)]
pub(super) trait IoSessionBridge: Send + Sync {
    fn route_completion(
        &self,
        io: &mut IoState,
        completion: WorkCompletion,
    ) -> Option<ConnectionToken>;

    fn dispatch_connection_completions(
        &self,
        io: &mut IoState,
        connection: ConnectionToken,
        quantum: usize,
    ) -> (usize, bool);

    fn handle_reclamation_deadline(&self, io: &mut IoState, token: OperationToken);

    fn commit_terminal_effects(&self, effects: IoCoreEffects);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IoDeadlineRequest {
    pub(super) at: tokio::time::Instant,
    pub(super) token: OperationToken,
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
    #[cfg(any(test, feature = "test-hooks"))]
    accepted_observation: Arc<Mutex<HashSet<OperationToken>>>,
    io_events: Mutex<Option<IoEventSender>>,
    drain_notify: Arc<tokio::sync::Notify>,
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
            #[cfg(any(test, feature = "test-hooks"))]
            accepted_observation: Arc::new(Mutex::new(HashSet::new())),
            io_events: Mutex::new(None),
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

    pub(super) fn drain_notify(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.drain_notify)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn accepted_tokens_for_observation(&self) -> Vec<OperationToken> {
        lock_unpoison(&self.accepted_observation)
            .iter()
            .copied()
            .collect()
    }

    pub(super) fn install_io_event_sender(
        &self,
        sender: IoEventSender,
        already_terminal: bool,
    ) -> Result<bool> {
        let mut current = lock_unpoison(&self.io_events);
        if current.is_some() {
            return Err(Error::InvalidConfig(
                "connection already has an attached I/O event port".into(),
            ));
        }

        *current = Some(sender);
        drop(current);
        Ok(already_terminal)
    }

    pub(super) fn pending_io_event(&self, event: IoTerminalEvent) -> Option<PendingIoEvent> {
        let sender = lock_unpoison(&self.io_events).clone();
        sender.map(|sender| sender.terminal(event))
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
pub(super) struct IoState {
    operations: OperationRegistry,
    cq_credits: CqCreditPool,
    connections: HashMap<ConnectionToken, ConnectionIoState>,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqes: u64,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqe_reasons: Vec<CqeReject>,
    pub(super) accepted_operations: usize,
    pub(super) pending_reclamations: usize,
    pub(super) quarantined_operations: usize,
    pub(super) quarantined_mrs: usize,
    pub(super) quarantined_bytes: usize,
    quarantined_operation_keys: HashMap<OperationToken, ConnectionToken>,
    quarantined_connection_counts: HashMap<ConnectionToken, usize>,
    pub(super) published_completion_connections: VecDeque<ConnectionToken>,
    published_completion_set: HashSet<ConnectionToken>,
    admission: Arc<RwLock<()>>,
    admission_error: Option<Error>,
    shutdown_requested: bool,
    driver_signal: Arc<dyn IoDriverSignal>,
    missing_cqe_deadline: Duration,
    completion_dispatch_budget: usize,
    reclamation_requests: VecDeque<IoDeadlineRequest>,
    terminal_failure: Option<super::lifecycle::MemoizedTerminalResult>,
}

#[cfg(test)]
pub(super) type IoCore = IoState;

struct ConnectionIoState {
    identity: EstablishedIoIdentity,
    max_send_wr: usize,
    max_recv_wr: usize,
    local_credits: LocalCredits,
    accepted: HashSet<AcceptedWrIdentity>,
    completions: VecDeque<WorkCompletion>,
    quarantined: bool,
    #[cfg(any(test, feature = "test-hooks"))]
    accepted_observation: Arc<Mutex<HashSet<OperationToken>>>,
}

impl ConnectionIoState {
    fn from_connection(connection: &EstablishedIoConnection) -> Self {
        Self {
            identity: connection.identity,
            max_send_wr: connection.max_send_wr,
            max_recv_wr: connection.max_recv_wr,
            local_credits: LocalCredits::default(),
            accepted: HashSet::new(),
            completions: VecDeque::new(),
            quarantined: false,
            #[cfg(any(test, feature = "test-hooks"))]
            accepted_observation: Arc::clone(&connection.accepted_observation),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

impl IoState {
    pub(super) fn new_owned(
        max_inflight_operations: usize,
        cq_capacity: usize,
        missing_cqe_deadline: Duration,
        completion_dispatch_budget: usize,
        admission: Arc<RwLock<()>>,
        driver_signal: Arc<dyn IoDriverSignal>,
    ) -> Result<Self> {
        Ok(Self {
            operations: OperationRegistry::new(max_inflight_operations)?,
            cq_credits: CqCreditPool::new(cq_capacity),
            connections: HashMap::new(),
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqes: 0,
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqe_reasons: Vec::new(),
            accepted_operations: 0,
            pending_reclamations: 0,
            quarantined_operations: 0,
            quarantined_mrs: 0,
            quarantined_bytes: 0,
            quarantined_operation_keys: HashMap::new(),
            quarantined_connection_counts: HashMap::new(),
            published_completion_connections: VecDeque::new(),
            published_completion_set: HashSet::new(),
            admission,
            admission_error: None,
            shutdown_requested: false,
            driver_signal,
            missing_cqe_deadline,
            completion_dispatch_budget,
            reclamation_requests: VecDeque::new(),
            terminal_failure: None,
        })
    }

    pub(super) fn admission_error(&self) -> Option<Error> {
        self.admission_error.clone()
    }

    pub(super) fn close_admission(&mut self, error: Option<Error>) {
        self.shutdown_requested = true;
        self.admission_error = error;
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

    fn publish_io_if_drained(&self, previous: usize) {
        if previous == 1 && self.shutdown_requested {
            self.driver_signal.publish_completion_dispatch();
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn pause_operation_before_register(&self) {
        self.driver_signal.pause_operation_before_register();
    }

    fn schedule_reclamation(&mut self, token: OperationToken) {
        self.begin_reclamation(token);
        let now = tokio::time::Instant::now();
        let at = now.checked_add(self.missing_cqe_deadline).unwrap_or(now);
        self.reclamation_requests
            .push_back(IoDeadlineRequest { at, token });
        self.publish_reclamation();
    }

    pub(in crate::v2::engine) fn cancel_operation(&mut self, token: OperationToken) {
        let Lookup::Occupied(operation) = self.operations.lookup_mut(token) else {
            return;
        };
        if operation.cancel_backend() {
            self.pending_reclamations += 1;
            self.schedule_reclamation(token);
        }
    }

    pub(super) fn take_reclamation_requests(&mut self, budget: usize) -> Vec<IoDeadlineRequest> {
        let count = self.reclamation_requests.len().min(budget);
        self.reclamation_requests.drain(..count).collect()
    }

    #[cfg(test)]
    pub(super) fn has_reclamation_requests(&self) -> bool {
        !self.reclamation_requests.is_empty()
    }

    pub(super) fn reclamation_request_count(&self) -> usize {
        self.reclamation_requests.len()
    }

    pub(super) fn diagnostics(&self) -> IoCoreDiagnostics {
        IoCoreDiagnostics {
            registered_operations: self.operations.live(),
            accepted_operations: self.accepted_operations,
            pending_reclamations: self.pending_reclamations,
            available_cq_credits: self.cq_credits.free(),
            retained_cq_credits: self.cq_credits.retained(),
            quarantined_operations: self.quarantined_operations,
            quarantined_mrs: self.quarantined_mrs,
            quarantined_bytes: self.quarantined_bytes,
        }
    }

    pub(super) fn accepted_count(&self) -> usize {
        self.accepted_operations
    }

    pub(super) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub(super) fn begin_terminal_failure(
        &mut self,
        outcome: super::lifecycle::MemoizedTerminalResult,
    ) {
        if self.terminal_failure.is_none() {
            self.terminal_failure = Some(outcome);
        }
    }

    pub(super) fn terminal_failure(&self) -> Option<super::lifecycle::MemoizedTerminalResult> {
        self.terminal_failure.clone()
    }

    pub(super) fn publish_connection(&mut self, connection: &Arc<EstablishedIoConnection>) {
        let token = connection.identity().connection;
        if self.published_completion_set.insert(token) {
            self.published_completion_connections.push_back(token);
        }
        self.publish_completion_dispatch();
    }

    pub(super) fn take_published_connection(&mut self) -> Option<ConnectionToken> {
        let connection = self.published_completion_connections.pop_front()?;
        self.published_completion_set.remove(&connection);
        Some(connection)
    }

    pub(super) fn has_published_connections(&self) -> bool {
        !self.published_completion_connections.is_empty()
    }

    pub(super) fn published_connection_count(&self) -> usize {
        self.published_completion_connections.len()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn rejected_cqe_reasons(&self) -> Vec<CqeReject> {
        self.rejected_cqe_reasons.clone()
    }

    fn with_connection_mut<R>(
        &mut self,
        connection: &EstablishedIoConnection,
        mutate: impl FnOnce(&mut ConnectionIoState) -> R,
    ) -> R {
        let state = self
            .connections
            .entry(connection.identity().connection)
            .or_insert_with(|| ConnectionIoState::from_connection(connection));
        mutate(state)
    }

    fn with_connection_token_mut<R>(
        &mut self,
        token: ConnectionToken,
        mutate: impl FnOnce(&mut ConnectionIoState) -> R,
    ) -> Option<R> {
        self.connections.get_mut(&token).map(mutate)
    }

    pub(in crate::v2::engine) fn retire_connection_io(&mut self, token: ConnectionToken) {
        let removed = self.connections.remove(&token);
        if let Some(state) = removed {
            debug_assert!(
                state.accepted.is_empty() && state.completions.is_empty(),
                "retired connection I/O state must have no provider-owned work"
            );
        }
        self.published_completion_set.remove(&token);
        self.published_completion_connections
            .retain(|queued| *queued != token);
    }

    pub(super) fn reserve_local(
        &mut self,
        connection: &EstablishedIoConnection,
        direction: Direction,
    ) -> Result<()> {
        self.with_connection_mut(connection, |state| {
            let (used, maximum) = match direction {
                Direction::Send => (&mut state.local_credits.send, state.max_send_wr),
                Direction::Recv => (&mut state.local_credits.recv, state.max_recv_wr),
            };
            if *used >= maximum {
                return Err(Error::CapacityExhausted);
            }
            *used += 1;
            Ok(())
        })
    }

    pub(super) fn release_local(
        &mut self,
        connection: &EstablishedIoConnection,
        direction: Direction,
    ) {
        self.with_connection_mut(connection, |state| {
            let used = match direction {
                Direction::Send => &mut state.local_credits.send,
                Direction::Recv => &mut state.local_credits.recv,
            };
            *used = used.saturating_sub(1);
        });
    }

    pub(super) fn add_accepted(
        &mut self,
        connection: &EstablishedIoConnection,
        token: OperationToken,
    ) {
        self.with_connection_mut(connection, |state| {
            state.accepted.insert(AcceptedWrIdentity {
                connection: state.identity.connection,
                qp_num: state.identity.qp_num,
                operation: token,
            });
            #[cfg(any(test, feature = "test-hooks"))]
            lock_unpoison(&connection.accepted_observation).insert(token);
        });
    }

    pub(super) fn remove_accepted(
        &mut self,
        connection: &EstablishedIoConnection,
        token: OperationToken,
    ) -> bool {
        let removed = self.with_connection_mut(connection, |state| {
            state.accepted.remove(&AcceptedWrIdentity {
                connection: state.identity.connection,
                qp_num: state.identity.qp_num,
                operation: token,
            })
        });
        #[cfg(any(test, feature = "test-hooks"))]
        debug_assert_eq!(
            removed,
            lock_unpoison(&connection.accepted_observation).remove(&token)
        );
        removed
    }

    pub(super) fn remove_operation_accepted(
        &mut self,
        identity: EstablishedIoIdentity,
        token: OperationToken,
    ) -> bool {
        self.with_connection_token_mut(identity.connection, |state| {
            if state.identity != identity {
                return false;
            }
            let removed = state.accepted.remove(&AcceptedWrIdentity {
                connection: identity.connection,
                qp_num: identity.qp_num,
                operation: token,
            });
            #[cfg(any(test, feature = "test-hooks"))]
            debug_assert_eq!(
                removed,
                lock_unpoison(&state.accepted_observation).remove(&token)
            );
            removed
        })
        .unwrap_or(false)
    }

    pub(super) fn add_operation_accepted(
        &mut self,
        identity: EstablishedIoIdentity,
        token: OperationToken,
    ) -> usize {
        self.with_connection_token_mut(identity.connection, |state| {
            if state.identity != identity {
                return state.accepted.len();
            }
            state.accepted.insert(AcceptedWrIdentity {
                connection: identity.connection,
                qp_num: identity.qp_num,
                operation: token,
            });
            #[cfg(any(test, feature = "test-hooks"))]
            lock_unpoison(&state.accepted_observation).insert(token);
            state.accepted.len()
        })
        .unwrap_or(0)
    }

    pub(super) fn release_operation_local(
        &mut self,
        identity: EstablishedIoIdentity,
        direction: Direction,
    ) {
        let _ = self.with_connection_token_mut(identity.connection, |state| {
            if state.identity != identity {
                return;
            }
            let used = match direction {
                Direction::Send => &mut state.local_credits.send,
                Direction::Recv => &mut state.local_credits.recv,
            };
            *used = used.saturating_sub(1);
        });
    }

    pub(super) fn operation_connection_accepted_count(
        &self,
        identity: EstablishedIoIdentity,
    ) -> usize {
        self.connections
            .get(&identity.connection)
            .filter(|state| state.identity == identity)
            .map_or(0, |state| state.accepted.len())
    }

    pub(super) fn accepted_tokens_bounded(
        &self,
        connection: &EstablishedIoConnection,
        limit: usize,
    ) -> Vec<OperationToken> {
        self.connections
            .get(&connection.identity().connection)
            .filter(|state| state.identity == connection.identity())
            .into_iter()
            .flat_map(|state| {
                state
                    .accepted
                    .iter()
                    .take(limit)
                    .map(|identity| identity.operation)
            })
            .collect()
    }

    pub(super) fn connection_accepted_count(&self, connection: &EstablishedIoConnection) -> usize {
        self.operation_connection_accepted_count(connection.identity())
    }

    pub(super) fn enqueue_completion(
        &mut self,
        connection: &EstablishedIoConnection,
        completion: WorkCompletion,
    ) {
        self.with_connection_mut(connection, |state| {
            state.completions.push_back(completion);
        });
    }

    pub(super) fn pop_completion(
        &mut self,
        connection: &EstablishedIoConnection,
    ) -> Option<WorkCompletion> {
        self.with_connection_mut(connection, |state| state.completions.pop_front())
    }

    pub(super) fn has_connection_completion_work(
        &self,
        connection: &EstablishedIoConnection,
    ) -> bool {
        self.connections
            .get(&connection.identity().connection)
            .filter(|state| state.identity == connection.identity())
            .is_some_and(|state| !state.completions.is_empty())
    }

    pub(super) fn begin_connection_quarantine(
        &mut self,
        connection: &EstablishedIoConnection,
    ) -> Option<IoQuarantineReport> {
        let state = self
            .connections
            .get_mut(&connection.identity().connection)?;
        let outstanding = state.accepted.len();
        if outstanding == 0 || std::mem::replace(&mut state.quarantined, true) {
            return None;
        }
        Some(IoQuarantineReport {
            outstanding_operations: outstanding,
            cq_debt: outstanding,
        })
    }

    fn record_operation_quarantine(
        &mut self,
        operation: OperationToken,
        connection: ConnectionToken,
    ) -> bool {
        let previous = self
            .quarantined_operation_keys
            .insert(operation, connection);
        debug_assert!(
            previous.is_none(),
            "operation quarantine key is single-shot"
        );
        let count = self
            .quarantined_connection_counts
            .entry(connection)
            .or_insert(0);
        let first = *count == 0;
        *count += 1;
        first
    }

    fn clear_operation_quarantine(
        &mut self,
        operation: OperationToken,
        connection: ConnectionToken,
    ) -> bool {
        let removed = self.quarantined_operation_keys.remove(&operation);
        debug_assert_eq!(
            removed,
            Some(connection),
            "operation quarantine clears its exact production key"
        );
        let Some(count) = self.quarantined_connection_counts.get_mut(&connection) else {
            debug_assert!(false, "quarantined connection count must exist");
            return false;
        };
        *count -= 1;
        if *count != 0 {
            return false;
        }
        self.quarantined_connection_counts.remove(&connection);
        true
    }

    #[cfg(test)]
    pub(super) fn operation_quarantined_for_test(&self, operation: OperationToken) -> bool {
        self.quarantined_operation_keys.contains_key(&operation)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestPostAuthority;

    struct RecordingSignal {
        io_publications: AtomicUsize,
    }

    impl IoDriverSignal for RecordingSignal {
        fn publish_cq_recheck(&self) {
            self.io_publications.fetch_add(1, Ordering::AcqRel);
        }

        fn publish_completion_dispatch(&self) {
            self.io_publications.fetch_add(1, Ordering::AcqRel);
        }

        fn publish_reclamation(&self) {
            self.io_publications.fetch_add(1, Ordering::AcqRel);
        }

        fn pause_operation_before_register(&self) {}
    }

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
        let mut core = IoCore::new_owned(
            8,
            8,
            Duration::ZERO,
            8,
            Arc::new(RwLock::new(())),
            Arc::new(RecordingSignal {
                io_publications: AtomicUsize::new(0),
            }),
        )
        .unwrap();
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

        core.reserve_local(&connection, Direction::Send).unwrap();
        assert!(matches!(
            core.reserve_local(&connection, Direction::Send),
            Err(Error::CapacityExhausted)
        ));
        core.release_local(&connection, Direction::Send);
        core.reserve_local(&connection, Direction::Recv).unwrap();

        let operation = OperationToken {
            slot: 7,
            generation: 11,
        };
        core.add_accepted(&connection, operation);
        assert_eq!(
            core.accepted_tokens_bounded(&connection, 8),
            vec![operation]
        );
        assert_eq!(core.connection_accepted_count(&connection), 1);

        core.enqueue_completion(&connection, WorkCompletion::default());
        assert!(core.has_connection_completion_work(&connection));
        assert!(core.pop_completion(&connection).is_some());
        assert!(!core.has_connection_completion_work(&connection));

        assert!(core.remove_accepted(&connection, operation));
        core.release_local(&connection, Direction::Recv);
    }

    #[test]
    fn final_accepted_drain_publishes_io_owner_reconsideration() {
        let signal = Arc::new(RecordingSignal {
            io_publications: AtomicUsize::new(0),
        });
        let mut core = IoCore::new_owned(
            1,
            1,
            Duration::ZERO,
            1,
            Arc::new(RwLock::new(())),
            Arc::clone(&signal) as Arc<dyn IoDriverSignal>,
        )
        .unwrap();
        core.close_admission(Some(Error::DriverShutdown));

        core.publish_io_if_drained(1);

        assert_eq!(signal.io_publications.load(Ordering::Acquire), 1);
    }
}
