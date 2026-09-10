//! Low-level operation/completion state composed by the v2 engine.

mod operation;
mod progress;

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use super::io::{IoEventSender, IoTerminalEvent, PendingIoEvent};
use super::registry::{ConnectionToken, Lookup, OperationToken, lock_unpoison};
use crate::v2::error::{Error, Result};
use crate::wc::WorkCompletion;
pub(super) use operation::CqeReject;
pub use operation::RdmaOperation;
pub(in crate::v2::engine) use operation::future::OperationCommand;
pub(super) use operation::{
    CommittedIoCoreEffects, IoCoreEffects, OperationObserver, OperationQuarantineEffect,
    post_io_recv_batch_into, post_io_send_into,
};
use operation::{CqCreditPool, OperationRegistry};
#[cfg(test)]
pub(super) use operation::{
    completion_for_driver_test, install_accepted_operation_for_driver_test,
    operation_future_for_io_lifetime_test, register_operation_waker_for_test,
};
pub(super) use progress::IoReactorSources;

/// Restricted publication surface from the I/O core to the explicit driver.
pub(super) trait IoDriverSignal: Send + Sync {
    fn notify_reactor(&self);
    #[cfg(any(test, feature = "test-hooks"))]
    fn pause_operation_before_register(&self);
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

/// Resource-free I/O identity, limits, and observers for one established
/// connection. Provider resources remain exclusively in `ConnectionState`.
pub(super) struct EstablishedIoConnection {
    identity: EstablishedIoIdentity,
    max_send_wr: usize,
    max_recv_wr: usize,
    #[cfg(any(test, feature = "test-hooks"))]
    accepted_observation: Arc<Mutex<Vec<OperationToken>>>,
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
        max_send_wr: usize,
        max_recv_wr: usize,
        drain_notify: Arc<tokio::sync::Notify>,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            max_send_wr,
            max_recv_wr,
            #[cfg(any(test, feature = "test-hooks"))]
            accepted_observation: Arc::new(Mutex::new(Vec::new())),
            io_events: Mutex::new(None),
            drain_notify,
        })
    }

    pub(super) fn identity(&self) -> EstablishedIoIdentity {
        self.identity
    }

    pub(super) fn drain_notify(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.drain_notify)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn accepted_tokens_for_observation(&self) -> Vec<OperationToken> {
        lock_unpoison(&self.accepted_observation).clone()
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

/// State owned by the low-level operation/completion runtime.
pub(super) struct IoState {
    operations: OperationRegistry,
    cq_credits: CqCreditPool,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqes: u64,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqe_reasons: Vec<CqeReject>,
    pub(super) accepted_operations: usize,
    pub(super) pending_reclamations: usize,
    pub(super) quarantined_operations: usize,
    pub(super) quarantined_mrs: usize,
    pub(super) quarantined_bytes: usize,
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

pub(in crate::v2::engine) struct ConnectionIoState {
    identity: EstablishedIoIdentity,
    max_send_wr: usize,
    max_recv_wr: usize,
    local_credits: LocalCredits,
    accepted: HashSet<OperationToken>,
    completions: VecDeque<WorkCompletion>,
    quarantined: HashSet<OperationToken>,
    #[cfg(any(test, feature = "test-hooks"))]
    accepted_observation: Arc<Mutex<Vec<OperationToken>>>,
}

impl ConnectionIoState {
    pub(in crate::v2::engine) fn from_connection(connection: &EstablishedIoConnection) -> Self {
        Self {
            identity: connection.identity,
            max_send_wr: connection.max_send_wr,
            max_recv_wr: connection.max_recv_wr,
            local_credits: LocalCredits::default(),
            accepted: HashSet::new(),
            completions: VecDeque::new(),
            quarantined: HashSet::new(),
            #[cfg(any(test, feature = "test-hooks"))]
            accepted_observation: Arc::clone(&connection.accepted_observation),
        }
    }

    pub(in crate::v2::engine) fn identity(&self) -> EstablishedIoIdentity {
        self.identity
    }

    pub(in crate::v2::engine) fn accepted_count(&self) -> usize {
        self.accepted.len()
    }

    pub(in crate::v2::engine) fn accepted_tokens_bounded(
        &self,
        limit: usize,
    ) -> Vec<OperationToken> {
        self.accepted.iter().take(limit).copied().collect()
    }

    pub(in crate::v2::engine) fn has_completion_work(&self) -> bool {
        !self.completions.is_empty()
    }

    pub(in crate::v2::engine) fn quarantined_operation_count(&self) -> usize {
        self.quarantined.len()
    }

    pub(in crate::v2::engine) fn mark_operation_quarantined(
        &mut self,
        operation: OperationToken,
    ) -> bool {
        assert!(
            self.accepted.contains(&operation),
            "only accepted work can enter connection quarantine"
        );
        self.quarantined.insert(operation)
    }

    pub(in crate::v2::engine) fn clear_operation_quarantined(
        &mut self,
        operation: OperationToken,
    ) -> bool {
        self.quarantined.remove(&operation) && self.quarantined.is_empty()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn insert_accepted_for_test(&mut self, operation: OperationToken) {
        assert!(self.accepted.insert(operation));
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
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqes: 0,
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqe_reasons: Vec::new(),
            accepted_operations: 0,
            pending_reclamations: 0,
            quarantined_operations: 0,
            quarantined_mrs: 0,
            quarantined_bytes: 0,
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

    fn notify_reactor(&self) {
        self.driver_signal.notify_reactor();
    }

    fn publish_io_if_drained(&self, previous: usize) {
        if previous == 1 && self.shutdown_requested {
            self.driver_signal.notify_reactor();
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
        self.notify_reactor();
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

    pub(in crate::v2::engine) fn operation_connection(
        &self,
        token: OperationToken,
    ) -> Option<ConnectionToken> {
        match self.operations.lookup(token) {
            Lookup::Occupied(operation) => Some(operation.connection_token()),
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => None,
        }
    }

    pub(super) fn take_reclamation_requests(&mut self, budget: usize) -> Vec<IoDeadlineRequest> {
        let count = self.reclamation_requests.len().min(budget);
        self.reclamation_requests.drain(..count).collect()
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

    pub(super) fn publish_connection(&mut self, connection: ConnectionToken) {
        let token = connection;
        if self.published_completion_set.insert(token) {
            self.published_completion_connections.push_back(token);
        }
        self.notify_reactor();
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

    pub(in crate::v2::engine) fn retire_connection_io(
        &mut self,
        token: ConnectionToken,
        state: &ConnectionIoState,
    ) {
        debug_assert_eq!(state.identity.connection, token);
        debug_assert!(
            state.accepted.is_empty() && state.completions.is_empty(),
            "retired connection I/O state must have no provider-owned work"
        );
        self.published_completion_set.remove(&token);
        self.published_completion_connections
            .retain(|queued| *queued != token);
    }

    pub(super) fn reserve_local(
        &mut self,
        connection: &EstablishedIoConnection,
        state: &mut ConnectionIoState,
        direction: Direction,
    ) -> Result<()> {
        debug_assert_eq!(state.identity, connection.identity());
        let (used, maximum) = match direction {
            Direction::Send => (&mut state.local_credits.send, state.max_send_wr),
            Direction::Recv => (&mut state.local_credits.recv, state.max_recv_wr),
        };
        if *used >= maximum {
            return Err(Error::CapacityExhausted);
        }
        *used += 1;
        Ok(())
    }

    pub(super) fn release_local(
        &mut self,
        connection: &EstablishedIoConnection,
        state: &mut ConnectionIoState,
        direction: Direction,
    ) {
        debug_assert_eq!(state.identity, connection.identity());
        Self::release_operation_local(state, connection.identity(), direction);
    }

    pub(super) fn add_accepted(
        &mut self,
        connection: &EstablishedIoConnection,
        state: &mut ConnectionIoState,
        token: OperationToken,
    ) {
        debug_assert_eq!(state.identity, connection.identity());
        let inserted = state.accepted.insert(token);
        debug_assert!(inserted, "accepted operation identity is single-shot");
        #[cfg(any(test, feature = "test-hooks"))]
        {
            let mut observed = lock_unpoison(&connection.accepted_observation);
            debug_assert!(!observed.contains(&token));
            observed.push(token);
        }
    }

    pub(super) fn remove_accepted(
        &mut self,
        connection: &EstablishedIoConnection,
        state: &mut ConnectionIoState,
        token: OperationToken,
    ) -> bool {
        debug_assert_eq!(state.identity, connection.identity());
        let removed = state.accepted.remove(&token);
        #[cfg(any(test, feature = "test-hooks"))]
        {
            let mut observed = lock_unpoison(&connection.accepted_observation);
            let observed_removed = observed
                .iter()
                .position(|candidate| *candidate == token)
                .map(|position| observed.swap_remove(position))
                .is_some();
            debug_assert_eq!(removed, observed_removed);
        }
        removed
    }

    pub(super) fn remove_operation_accepted(
        state: &mut ConnectionIoState,
        identity: EstablishedIoIdentity,
        token: OperationToken,
    ) -> bool {
        if state.identity != identity {
            return false;
        }
        let removed = state.accepted.remove(&token);
        #[cfg(any(test, feature = "test-hooks"))]
        {
            let mut observed = lock_unpoison(&state.accepted_observation);
            let observed_removed = observed
                .iter()
                .position(|candidate| *candidate == token)
                .map(|position| observed.swap_remove(position))
                .is_some();
            debug_assert_eq!(removed, observed_removed);
        }
        removed
    }

    pub(super) fn add_operation_accepted(
        state: &mut ConnectionIoState,
        identity: EstablishedIoIdentity,
        token: OperationToken,
    ) -> usize {
        if state.identity == identity {
            state.accepted.insert(token);
            #[cfg(any(test, feature = "test-hooks"))]
            {
                let mut observed = lock_unpoison(&state.accepted_observation);
                if !observed.contains(&token) {
                    observed.push(token);
                }
            }
        }
        state.accepted.len()
    }

    pub(super) fn release_operation_local(
        state: &mut ConnectionIoState,
        identity: EstablishedIoIdentity,
        direction: Direction,
    ) {
        if state.identity != identity {
            return;
        }
        let used = match direction {
            Direction::Send => &mut state.local_credits.send,
            Direction::Recv => &mut state.local_credits.recv,
        };
        *used = used.saturating_sub(1);
    }

    pub(super) fn operation_connection_accepted_count(
        state: &ConnectionIoState,
        identity: EstablishedIoIdentity,
    ) -> usize {
        if state.identity == identity {
            state.accepted.len()
        } else {
            0
        }
    }

    #[cfg(test)]
    pub(super) fn accepted_tokens_bounded(
        &self,
        connection: &EstablishedIoConnection,
        state: &ConnectionIoState,
        limit: usize,
    ) -> Vec<OperationToken> {
        if state.identity != connection.identity() {
            return Vec::new();
        }
        state.accepted_tokens_bounded(limit)
    }

    #[cfg(test)]
    pub(super) fn connection_accepted_count(
        &self,
        connection: &EstablishedIoConnection,
        state: &ConnectionIoState,
    ) -> usize {
        Self::operation_connection_accepted_count(state, connection.identity())
    }

    pub(super) fn enqueue_completion(
        &mut self,
        connection: &EstablishedIoConnection,
        state: &mut ConnectionIoState,
        completion: WorkCompletion,
    ) {
        debug_assert_eq!(state.identity, connection.identity());
        state.completions.push_back(completion);
    }

    pub(super) fn pop_completion(
        &mut self,
        connection: &EstablishedIoConnection,
        state: &mut ConnectionIoState,
    ) -> Option<WorkCompletion> {
        debug_assert_eq!(state.identity, connection.identity());
        state.completions.pop_front()
    }

    pub(super) fn has_connection_completion_work(
        &self,
        connection: &EstablishedIoConnection,
        state: &ConnectionIoState,
    ) -> bool {
        state.identity == connection.identity() && state.has_completion_work()
    }

    pub(super) fn begin_connection_quarantine(
        &mut self,
        connection: &EstablishedIoConnection,
        state: &mut ConnectionIoState,
    ) -> Option<IoQuarantineReport> {
        if state.identity != connection.identity() {
            return None;
        }
        let outstanding = state.accepted.len();
        if outstanding == 0 {
            return None;
        }
        Some(IoQuarantineReport {
            outstanding_operations: outstanding,
            cq_debt: outstanding,
        })
    }

    fn clear_operation_quarantine(
        &mut self,
        state: &mut ConnectionIoState,
        operation: OperationToken,
        connection: ConnectionToken,
    ) -> bool {
        debug_assert_eq!(state.identity.connection, connection);
        debug_assert!(
            state.quarantined.contains(&operation),
            "cleared quarantine must name an exact retained operation"
        );
        state.quarantined.contains(&operation)
    }

    #[cfg(test)]
    pub(super) fn operation_quarantined_for_test(
        &self,
        state: &ConnectionIoState,
        operation: OperationToken,
    ) -> bool {
        state.quarantined.contains(&operation)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct RecordingSignal {
        io_publications: AtomicUsize,
    }

    impl IoDriverSignal for RecordingSignal {
        fn notify_reactor(&self) {
            self.io_publications.fetch_add(1, Ordering::AcqRel);
        }

        fn pause_operation_before_register(&self) {}
    }

    #[test]
    fn value_owned_connection_ledger_tracks_local_accepted_and_completion_state() {
        let mut core = IoState::new_owned(
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
            1,
            1,
            Arc::new(tokio::sync::Notify::new()),
        );
        let mut connection_io = ConnectionIoState::from_connection(&connection);

        core.reserve_local(&connection, &mut connection_io, Direction::Send)
            .unwrap();
        assert!(matches!(
            core.reserve_local(&connection, &mut connection_io, Direction::Send),
            Err(Error::CapacityExhausted)
        ));
        core.release_local(&connection, &mut connection_io, Direction::Send);
        core.reserve_local(&connection, &mut connection_io, Direction::Recv)
            .unwrap();

        let operation = OperationToken {
            slot: 7,
            generation: 11,
        };
        core.add_accepted(&connection, &mut connection_io, operation);
        assert_eq!(
            core.accepted_tokens_bounded(&connection, &connection_io, 8),
            vec![operation]
        );
        assert_eq!(
            core.connection_accepted_count(&connection, &connection_io),
            1
        );

        core.enqueue_completion(&connection, &mut connection_io, WorkCompletion::default());
        assert!(core.has_connection_completion_work(&connection, &connection_io));
        assert!(
            core.pop_completion(&connection, &mut connection_io)
                .is_some()
        );
        assert!(!core.has_connection_completion_work(&connection, &connection_io));

        assert!(core.remove_accepted(&connection, &mut connection_io, operation));
        core.release_local(&connection, &mut connection_io, Direction::Recv);
    }

    #[test]
    fn final_accepted_drain_publishes_io_owner_reconsideration() {
        let signal = Arc::new(RecordingSignal {
            io_publications: AtomicUsize::new(0),
        });
        let mut core = IoState::new_owned(
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
