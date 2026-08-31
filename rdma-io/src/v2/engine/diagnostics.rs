use super::config::CompletionMode;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Concurrently readable snapshot of engine state and configured capacity.
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
    /// Number of engine-owned context facades.
    pub shared_contexts: usize,
    /// Number of engine-owned protection domains.
    pub shared_protection_domains: usize,
    /// Number of engine-owned completion queues.
    pub shared_completion_queues: usize,
    /// Number of engine-owned CQ completion channels.
    pub shared_completion_channels: usize,
    /// Number of engine-owned CM event channels.
    pub shared_cm_event_channels: usize,
    /// Monotonic cooperative-yield count in polling mode.
    pub driver_yields: u64,
    /// Current aggregate connection reservations.
    pub live_connection_reservations: usize,
    /// Connection slots still available for this engine instance.
    pub free_connection_slots: usize,
    /// Connection slots permanently retired after generation exhaustion.
    pub retired_connection_slots: usize,
    /// Current operation registrations, including retained quarantined WRs.
    pub registered_operations: usize,
    /// Operation slots still available for this engine instance.
    pub free_operation_slots: usize,
    /// Operation slots permanently retired after generation exhaustion.
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
    /// Connections currently queued for bounded driver-local work.
    pub ready_queue_depth: usize,
    /// WRs for which admission/posting was attempted.
    pub operations_offered: u64,
    /// WRs accepted or conservatively treated as acceptance-ambiguous.
    pub operations_accepted: u64,
    /// WRs proven provider-unaccepted through a valid `bad_wr`.
    pub operations_unaccepted: u64,
    /// WRs successfully offered to the provider.
    pub operations_posted: u64,
    /// Exact validated CQEs consumed.
    pub operations_completed: u64,
    /// Posted operation futures dropped by callers.
    pub operations_cancelled: u64,
    /// Batch verbs calls attempted.
    pub batch_posts_attempted: u64,
    /// WRs transferred from accepted batch prefixes.
    pub batch_accepted_prefix: u64,
    /// WRs rolled back from proven-unaccepted suffixes.
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
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub accepted_test_operations: usize,
}

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

#[derive(Default)]
pub(super) struct DiagnosticsState {
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
}
