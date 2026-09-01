use super::EngineShared;
use super::config::CompletionMode;
use super::connection::RdmaConnectionIdentity;
use std::net::SocketAddr;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Public engine lifecycle state included in diagnostics snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdmaEngineLifecycle {
    /// Resources exist, but the driver has not been polled.
    Created,
    /// The sole engine driver is actively responsible for progress.
    Running,
    /// Admission is closed and the driver is draining existing work.
    ShutdownRequested,
    /// Graceful shutdown completed.
    Terminated,
    /// Progress ended with a terminal engine-wide failure.
    Failed,
}

/// Non-owning terminal error summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RdmaEngineTerminalError {
    /// Stable error-class name suitable for diagnostics.
    pub class: String,
    /// Contextual error message without registered-memory contents.
    pub message: String,
}

/// Per-connection state included in an engine diagnostics snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RdmaConnectionDiagnostics {
    /// Current generational connection and provider QP identity.
    pub identity: RdmaConnectionIdentity,
    /// Exact provider-accepted WRs still awaiting validated CQEs.
    pub accepted_outstanding_operations: usize,
    /// Whether posting has stopped and ordered drain has started.
    pub draining: bool,
    /// Whether this connection owns any retained quarantine state.
    pub quarantined: bool,
}

/// Per-listener queue state included in an engine diagnostics snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RdmaListenerDiagnostics {
    /// Stable engine-local listener token.
    pub token: u64,
    /// Bound listener address.
    pub local_addr: SocketAddr,
    /// Admitted children waiting in arrival order.
    pub queued_inbound_requests: usize,
    /// Unselected accept waiters waiting in registration order.
    pub pending_accepts: usize,
    /// Selected/setup or accepted-but-cleaning pairs.
    pub selected_accepts: usize,
}

/// Concurrently readable snapshot of engine state and configured capacity.
///
/// Creating the aggregate snapshot is constant-time with respect to registered
/// connections and listeners. Per-object detail is collected only when
/// [`Self::connections`] or [`Self::listeners`] is called.
///
/// # Use case
///
/// Observe engine lifecycle, capacity, routing rejects, and retained safety
/// state without stopping the driver.
///
/// # Ownership and progress
///
/// Aggregate fields are copied. Explicit detail methods use a weak engine
/// reference and do not keep the engine alive.
///
/// # Safety and limits
///
/// Aggregate collection is O(1); `connections()` and `listeners()` are O(N)
/// detail queries and expose no raw resource identity.
///
/// # Availability
///
/// Available with the `tokio` feature through `RdmaEngine::diagnostics`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RdmaEngineDiagnostics {
    /// Current monotonic engine lifecycle state.
    pub lifecycle: RdmaEngineLifecycle,
    /// Terminal error summary, if the engine failed.
    pub terminal_error: Option<RdmaEngineTerminalError>,
    /// Kernel RDMA device selected by the builder.
    pub device_name: String,
    /// Shared-CQ progress mode.
    pub completion_mode: CompletionMode,
    /// Configured aggregate connection admission limit.
    pub maximum_live_connections: usize,
    /// Configured global operation limit.
    pub maximum_inflight_operations: usize,
    /// Configured shared-CQ capacity.
    pub cq_capacity: usize,
    /// Maximum CQEs routed in one CQ service turn.
    pub cq_completion_budget: usize,
    /// Maximum CM actions handled in one CM service turn.
    pub cm_event_budget: usize,
    /// Maximum reclamation/deadline actions handled in one service turn.
    pub reclamation_budget: usize,
    /// Maximum connection-local work handled before ready-queue rotation.
    pub ready_connection_quantum: usize,
    /// Number of engine-owned context facades.
    pub shared_contexts: usize,
    /// Number of engine-owned protection domains.
    pub shared_protection_domains: usize,
    /// Number of engine-owned completion queues.
    pub shared_completion_queues: usize,
    /// Number of engine-owned CQ completion channels.
    pub shared_completion_channels: usize,
    /// Number of shared CQ notification file descriptors.
    pub shared_cq_notification_fds: usize,
    /// Number of engine-owned CM event channels.
    pub shared_cm_event_channels: usize,
    /// Number of shared CM event-channel file descriptors.
    pub shared_cm_event_fds: usize,
    /// Declarative number of driver futures returned by the one successful
    /// `build()` call, fixed at one by construction.
    pub explicit_engine_drivers: usize,
    /// Declarative engine design invariant, fixed at zero by construction and
    /// independently guarded by the source-level no-hidden-spawn test.
    pub library_owned_tasks: usize,
    /// Monotonic software producer wake count.
    pub driver_wakeups: u64,
    /// Monotonic cooperative-yield count in polling mode.
    pub driver_yields: u64,
    /// Current aggregate connection reservations.
    pub live_connection_reservations: usize,
    /// Reservations admitted before a connection registration becomes live.
    pub establishing_connection_reservations: usize,
    /// Registered reservations that are neither draining nor quarantined.
    pub established_connection_reservations: usize,
    /// Reservations whose connections have stopped posting and are draining.
    pub draining_connection_reservations: usize,
    /// Registered QPs that are not currently quarantined.
    pub registered_live_qps: usize,
    /// Connection slots still available for this engine instance.
    pub free_connection_slots: usize,
    /// Connection slots permanently retired after generation exhaustion.
    ///
    /// Retirement never reverses, so this gauge is also the monotonic
    /// connection-slot retirement counter.
    pub retired_connection_slots: usize,
    /// Current operation registrations, including retained quarantined WRs.
    pub registered_operations: usize,
    /// Operation slots still available for this engine instance.
    pub free_operation_slots: usize,
    /// Operation slots permanently retired after generation exhaustion.
    ///
    /// Retirement never reverses, so this gauge is also the monotonic
    /// operation-slot retirement counter.
    pub retired_operation_slots: usize,
    /// Provider-accepted or acceptance-ambiguous WRs awaiting exact CQEs.
    pub accepted_outstanding_operations: usize,
    /// CQ admission credits currently available.
    pub free_cq_credits: usize,
    /// CQ credits retained by quarantined operations.
    pub retained_cq_credits: usize,
    /// Cancelled operations waiting for their reclamation deadline or CQE.
    pub pending_reclamations: usize,
    /// Operation records retained after a missing-CQE deadline.
    pub quarantined_operations: usize,
    /// Registered MRs retained by quarantined operations.
    pub quarantined_mrs: usize,
    /// Bytes retained by quarantined operation MRs.
    pub quarantined_bytes: usize,
    /// Unique quarantined connection bundles.
    ///
    /// Each bundle retains its QP registration, connection admission
    /// reservation, and unresolved operation/CQ debt.
    pub quarantined_bundles: usize,
    /// Age of the oldest currently retained quarantine entry.
    pub oldest_quarantine_age: Option<Duration>,
    /// Connections currently queued for bounded driver-local work.
    pub ready_queue_depth: usize,
    /// Engine-owned listeners currently registered with the shared CM router.
    pub listener_count: usize,
    /// Admitted unmatched inbound children across all listeners.
    pub queued_inbound_requests: usize,
    /// Unselected accept waiters across all listeners.
    pub pending_accepts: usize,
    /// Listener-local selected/setup or accepted-but-cleaning pairs.
    pub selected_accepts: usize,
    /// Connection admission reservations successfully acquired.
    pub connections_admitted: u64,
    /// WRs for which admission/posting was attempted.
    pub operations_offered: u64,
    /// WRs accepted or conservatively treated as acceptance-ambiguous.
    pub operations_accepted: u64,
    /// WRs proven provider-unaccepted through a valid `bad_wr`.
    pub operations_unaccepted: u64,
    /// WRs provider-accepted or conservatively retained as acceptance-ambiguous.
    pub operations_posted: u64,
    /// Exact validated CQEs consumed.
    pub operations_completed: u64,
    /// Posted operation futures dropped by callers.
    pub operations_cancelled: u64,
    /// Batch verbs calls attempted.
    pub batch_posts_attempted: u64,
    /// WRs transferred from provider-proven accepted batch prefixes.
    pub batch_accepted_prefix: u64,
    /// WRs rolled back from provider-proven unaccepted batch suffixes.
    pub batch_unaccepted_suffix: u64,
    /// Batch calls whose acceptance could not be proven from `bad_wr`.
    pub batch_ambiguous_results: u64,
    /// CQEs polled from the shared CQ.
    pub cqes_polled: u64,
    /// CQEs delivered to their exact current operation.
    pub cqes_routed: u64,
    /// CQEs rejected because their connection generation was stale.
    pub stale_connection_cqes: u64,
    /// CQEs rejected because their operation generation was stale.
    pub stale_operation_cqes: u64,
    /// CQEs rejected because no current operation was known.
    pub unknown_cqes: u64,
    /// CQEs rejected as duplicate delivery.
    pub duplicate_cqes: u64,
    /// CQEs whose operation belonged to another connection.
    pub wrong_connection_cqes: u64,
    /// CQEs whose `qp_num` did not exactly match the operation registration.
    pub wrong_qp_num_cqes: u64,
    /// Successful CQEs with an unexpected opcode.
    pub unexpected_opcode_cqes: u64,
    /// Missing-CQE reclamation deadlines reached.
    pub reclamation_deadlines: u64,
    /// Capacity failures for connection registration.
    pub connection_capacity_exhausted: u64,
    /// Capacity failures for operation registration.
    pub operation_capacity_exhausted: u64,
    /// Capacity failures for CQ admission.
    pub cq_capacity_exhausted: u64,
    /// Inbound and outbound connections that reached RDMA-CM ESTABLISHED.
    pub connections_opened: u64,
    /// Connections that atomically stopped posting and entered drain.
    pub connections_drain_started: u64,
    /// Connections whose accepted outstanding set reached zero.
    pub connections_drained: u64,
    /// Connections that completed QP/CM destruction and generation retirement.
    pub connections_closed: u64,
    /// Connections retained in whole-bundle quarantine.
    pub connections_quarantined: u64,
    /// Connections or requests terminated by CM/setup/retirement failure.
    pub connections_failed: u64,
    /// Local QP transitions to ERR initiated by lifecycle teardown.
    pub qp_error_transitions: u64,
    /// Synchronous consuming `rdma_destroy_qp` invocations at zero outstanding.
    pub qp_destroys: u64,
    /// Quarantined connections later recovered by exact CQE routing.
    pub quarantine_recoveries: u64,
    /// Connection-local `ConnectionQuarantined` outcomes published.
    pub connection_quarantine_outcomes: u64,
    /// Graceful shutdown requests accepted by the admission barrier.
    pub shutdowns: u64,
    /// Engine-wide `EngineWedged` terminal outcomes.
    pub engine_wedges: u64,
    /// Terminal driver failures published to engine frontends.
    pub terminal_driver_errors: u64,
    /// CQ admission credits reserved before provider posting.
    pub cq_credits_reserved: u64,
    /// CQ credits rolled back for provider-proven unaccepted WRs.
    pub cq_credits_rolled_back: u64,
    /// CQ credits released after exact validated CQEs.
    pub cq_credits_released: u64,
    /// CQ credits marked retained by quarantine.
    pub cq_credits_retained: u64,
    /// Listeners successfully created on the shared CM channel.
    pub listeners_created: u64,
    /// Inbound children that reached RDMA-CM ESTABLISHED.
    pub inbound_requests_accepted: u64,
    /// Inbound children rejected by the engine.
    pub inbound_requests_rejected: u64,
    /// Inbound children rejected because the userspace backlog was full.
    pub inbound_rejected_backlog_full: u64,
    /// Inbound children rejected because aggregate connection capacity was full.
    pub inbound_rejected_connection_capacity: u64,
    /// Inbound children rejected after engine admission closed.
    pub inbound_rejected_admission_closed: u64,
    /// Inbound children rejected because their listener was closing.
    pub inbound_rejected_listener_closed: u64,
    /// Inbound children rejected for exact verbs-context mismatch.
    pub inbound_rejected_context_mismatch: u64,
    /// Selected inbound children rejected after setup failure.
    pub inbound_rejected_setup_failure: u64,
    /// Accept futures cancelled before selection.
    pub accept_cancellations_before_selection: u64,
    /// Accept futures cancelled after selection.
    pub accept_cancellations_after_selection: u64,
    /// Selected pre-establishment setup failures.
    pub accept_setup_failures: u64,
    /// CM events consumed and acknowledged by the sole engine driver.
    pub cm_events_processed: u64,
    /// CM events rejected from live routing.
    pub cm_events_rejected: u64,
    /// CM events carrying a stale route generation.
    pub stale_cm_events: u64,
    /// Duplicate CM events for a completed transition.
    pub duplicate_cm_events: u64,
    /// CM events with no current engine route.
    pub unknown_cm_events: u64,
    /// CM events whose ID did not match their route token.
    pub wrong_id_cm_events: u64,
    /// CM events that were invalid for the route's current state.
    pub unexpected_cm_events: u64,
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub accepted_test_operations: usize,
    pub(super) detail_source: DiagnosticsDetailSource,
}

impl RdmaEngineDiagnostics {
    /// Collect exact state for each currently registered connection.
    ///
    /// This explicit detail query is O(number of registered connections);
    /// unlike [`crate::v2::RdmaEngine::diagnostics`], it intentionally visits
    /// connection registrations and locks their accepted-WR sets. It returns
    /// an empty vector if every engine handle has been dropped and the
    /// snapshot's internal weak detail source can no longer be upgraded; that
    /// case is indistinguishable from an engine with no registered connections.
    pub fn connections(&self) -> Vec<RdmaConnectionDiagnostics> {
        self.detail_source
            .upgrade()
            .map_or_else(Vec::new, |shared| shared.connection_diagnostics())
    }

    /// Collect queue state for each currently registered listener.
    ///
    /// This explicit detail query is O(number of listeners). Entries are
    /// ordered by listener token. It returns an empty vector if every engine
    /// handle has been dropped and the snapshot's internal weak detail source
    /// can no longer be upgraded.
    pub fn listeners(&self) -> Vec<RdmaListenerDiagnostics> {
        self.detail_source
            .upgrade()
            .map_or_else(Vec::new, |shared| shared.listener_diagnostics())
    }
}

#[derive(Clone, Debug)]
pub(super) struct DiagnosticsDetailSource(pub(super) Weak<EngineShared>);

impl DiagnosticsDetailSource {
    fn upgrade(&self) -> Option<std::sync::Arc<EngineShared>> {
        self.0.upgrade()
    }
}

impl PartialEq for DiagnosticsDetailSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for DiagnosticsDetailSource {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CqeReject {
    StaleConnection,
    StaleOperation,
    Unknown,
    Duplicate,
    WrongConnection,
    WrongQpNum,
    UnexpectedOpcode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CmEventReject {
    Stale,
    Duplicate,
    Unknown,
    WrongId,
    Unexpected,
}

#[derive(Default)]
pub(super) struct DiagnosticsState {
    pub(super) connections_admitted: AtomicU64,
    pub(super) operations_offered: AtomicU64,
    pub(super) operations_accepted: AtomicU64,
    pub(super) operations_unaccepted: AtomicU64,
    pub(super) operations_posted: AtomicU64,
    pub(super) operations_completed: AtomicU64,
    pub(super) operations_cancelled: AtomicU64,
    pub(super) batch_posts_attempted: AtomicU64,
    pub(super) batch_accepted_prefix: AtomicU64,
    pub(super) batch_unaccepted_suffix: AtomicU64,
    pub(super) batch_ambiguous_results: AtomicU64,
    pub(super) cqes_polled: AtomicU64,
    pub(super) cqes_routed: AtomicU64,
    pub(super) stale_connection_cqes: AtomicU64,
    pub(super) stale_operation_cqes: AtomicU64,
    pub(super) unknown_cqes: AtomicU64,
    pub(super) duplicate_cqes: AtomicU64,
    pub(super) wrong_connection_cqes: AtomicU64,
    pub(super) wrong_qp_num_cqes: AtomicU64,
    pub(super) unexpected_opcode_cqes: AtomicU64,
    pub(super) reclamation_deadlines: AtomicU64,
    pub(super) connection_capacity_exhausted: AtomicU64,
    pub(super) operation_capacity_exhausted: AtomicU64,
    pub(super) cq_capacity_exhausted: AtomicU64,
    pub(super) connections_opened: AtomicU64,
    pub(super) connections_drain_started: AtomicU64,
    pub(super) connections_drained: AtomicU64,
    pub(super) connections_closed: AtomicU64,
    pub(super) connections_quarantined: AtomicU64,
    pub(super) connections_failed: AtomicU64,
    pub(super) qp_error_transitions: AtomicU64,
    pub(super) qp_destroys: AtomicU64,
    pub(super) quarantine_recoveries: AtomicU64,
    pub(super) connection_quarantine_outcomes: AtomicU64,
    pub(super) shutdowns: AtomicU64,
    pub(super) engine_wedges: AtomicU64,
    pub(super) terminal_driver_errors: AtomicU64,
    pub(super) cq_credits_reserved: AtomicU64,
    pub(super) cq_credits_rolled_back: AtomicU64,
    pub(super) cq_credits_released: AtomicU64,
    pub(super) cq_credits_retained: AtomicU64,
    pub(super) listeners_created: AtomicU64,
    pub(super) inbound_requests_accepted: AtomicU64,
    pub(super) inbound_requests_rejected: AtomicU64,
    pub(super) inbound_rejected_backlog_full: AtomicU64,
    pub(super) inbound_rejected_connection_capacity: AtomicU64,
    pub(super) inbound_rejected_admission_closed: AtomicU64,
    pub(super) inbound_rejected_listener_closed: AtomicU64,
    pub(super) inbound_rejected_context_mismatch: AtomicU64,
    pub(super) inbound_rejected_setup_failure: AtomicU64,
    pub(super) accept_cancellations_before_selection: AtomicU64,
    pub(super) accept_cancellations_after_selection: AtomicU64,
    pub(super) accept_setup_failures: AtomicU64,
    pub(super) cm_events_processed: AtomicU64,
    pub(super) cm_events_rejected: AtomicU64,
    pub(super) stale_cm_events: AtomicU64,
    pub(super) duplicate_cm_events: AtomicU64,
    pub(super) unknown_cm_events: AtomicU64,
    pub(super) wrong_id_cm_events: AtomicU64,
    pub(super) unexpected_cm_events: AtomicU64,
}

impl DiagnosticsState {
    pub(super) fn reject_cqe(&self, reject: CqeReject) {
        let counter = match reject {
            CqeReject::StaleConnection => &self.stale_connection_cqes,
            CqeReject::StaleOperation => &self.stale_operation_cqes,
            CqeReject::Unknown => &self.unknown_cqes,
            CqeReject::Duplicate => &self.duplicate_cqes,
            CqeReject::WrongConnection => &self.wrong_connection_cqes,
            CqeReject::WrongQpNum => &self.wrong_qp_num_cqes,
            CqeReject::UnexpectedOpcode => &self.unexpected_opcode_cqes,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn reject_cm_event(&self, reject: CmEventReject) {
        self.cm_events_rejected.fetch_add(1, Ordering::Relaxed);
        let counter = match reject {
            CmEventReject::Stale => &self.stale_cm_events,
            CmEventReject::Duplicate => &self.duplicate_cm_events,
            CmEventReject::Unknown => &self.unknown_cm_events,
            CmEventReject::WrongId => &self.wrong_id_cm_events,
            CmEventReject::Unexpected => &self.unexpected_cm_events,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::engine::{CompletionMode, test_engine_pair};

    #[test]
    fn snapshot_keeps_capacity_resource_lifecycle_and_reject_counters_distinct() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let counters = &engine.shared.diagnostic_counters;
        counters.connections_admitted.store(2, Ordering::Release);
        counters.operations_offered.store(11, Ordering::Release);
        counters.operations_accepted.store(10, Ordering::Release);
        counters.operations_unaccepted.store(1, Ordering::Release);
        counters.stale_connection_cqes.store(3, Ordering::Release);
        counters.stale_operation_cqes.store(5, Ordering::Release);
        counters.unknown_cqes.store(7, Ordering::Release);
        counters.duplicate_cqes.store(9, Ordering::Release);
        counters.wrong_connection_cqes.store(11, Ordering::Release);
        counters.wrong_qp_num_cqes.store(13, Ordering::Release);
        counters.unexpected_opcode_cqes.store(15, Ordering::Release);
        counters.stale_cm_events.store(17, Ordering::Release);
        counters.unknown_cm_events.store(19, Ordering::Release);
        counters.duplicate_cm_events.store(21, Ordering::Release);
        counters.terminal_driver_errors.store(23, Ordering::Release);

        let diagnostics = engine.diagnostics();
        assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Created);
        assert_eq!(diagnostics.completion_mode, CompletionMode::Polling);
        assert_eq!(diagnostics.maximum_live_connections, 256);
        assert_eq!(diagnostics.maximum_inflight_operations, 16_384);
        assert_eq!(diagnostics.cq_capacity, 16_384);
        assert_eq!(diagnostics.cq_completion_budget, 32);
        assert_eq!(diagnostics.cm_event_budget, 32);
        assert_eq!(diagnostics.reclamation_budget, 32);
        assert_eq!(diagnostics.ready_connection_quantum, 32);
        assert_eq!(diagnostics.shared_completion_queues, 1);
        assert_eq!(diagnostics.shared_cq_notification_fds, 0);
        assert_eq!(diagnostics.shared_cm_event_fds, 1);
        assert_eq!(diagnostics.explicit_engine_drivers, 1);
        assert_eq!(diagnostics.library_owned_tasks, 0);
        assert_eq!(diagnostics.connections_admitted, 2);
        assert_eq!(diagnostics.operations_offered, 11);
        assert_eq!(diagnostics.operations_accepted, 10);
        assert_eq!(diagnostics.operations_unaccepted, 1);
        assert_eq!(diagnostics.stale_connection_cqes, 3);
        assert_eq!(diagnostics.stale_operation_cqes, 5);
        assert_eq!(diagnostics.unknown_cqes, 7);
        assert_eq!(diagnostics.duplicate_cqes, 9);
        assert_eq!(diagnostics.wrong_connection_cqes, 11);
        assert_eq!(diagnostics.wrong_qp_num_cqes, 13);
        assert_eq!(diagnostics.unexpected_opcode_cqes, 15);
        assert_eq!(diagnostics.stale_cm_events, 17);
        assert_eq!(diagnostics.unknown_cm_events, 19);
        assert_eq!(diagnostics.duplicate_cm_events, 21);
        assert_eq!(diagnostics.terminal_driver_errors, 23);
        assert!(diagnostics.connections().is_empty());
        assert!(diagnostics.listeners().is_empty());
        assert_eq!(diagnostics.oldest_quarantine_age, None);
        drop(driver);
    }

    #[test]
    fn snapshot_equality_ignores_the_internal_weak_detail_source() {
        let (first_engine, first_driver) = test_engine_pair(CompletionMode::Polling);
        let (second_engine, second_driver) = test_engine_pair(CompletionMode::Polling);

        assert_eq!(first_engine.diagnostics(), second_engine.diagnostics());

        drop(first_driver);
        drop(second_driver);
    }

    #[test]
    fn detail_queries_are_empty_after_the_last_engine_owner_is_gone() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let diagnostics = engine.diagnostics();

        drop(engine);
        drop(driver);

        assert!(diagnostics.connections().is_empty());
        assert!(diagnostics.listeners().is_empty());
    }
}
