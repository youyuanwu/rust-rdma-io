/// Lifecycle of the explicitly driven RDMA engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdmaEngineLifecycle {
    /// Resources exist, but the driver has not been polled.
    Created,
    /// The engine driver is responsible for progress.
    Running,
    /// Admission is closed and existing work is draining.
    ShutdownRequested,
    /// Graceful shutdown completed.
    Terminated,
    /// Progress ended with an engine-wide failure.
    Failed,
}

/// Non-owning summary of an engine-wide terminal error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RdmaEngineTerminalError {
    /// Stable error-class name.
    pub class: String,
    /// Contextual error message without registered-memory contents.
    pub message: String,
}

/// Compact snapshot of engine health and hardware-ownership debt.
///
/// The snapshot intentionally excludes configuration echoes, scheduler
/// counters, per-object listings, and event ledgers. Operation results carry
/// contextual failures; this type exists only to answer whether the engine is
/// live, draining, or retaining resources because safe release is unproven.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RdmaEngineDiagnostics {
    /// Current monotonic engine lifecycle.
    pub lifecycle: RdmaEngineLifecycle,
    /// Engine-wide terminal failure, if any.
    pub terminal_error: Option<RdmaEngineTerminalError>,
    /// Current registered connection count, including retained connections.
    pub live_connections: usize,
    /// Current operation registrations, including retained operations.
    pub registered_operations: usize,
    /// Provider-accepted or acceptance-ambiguous operations awaiting proof.
    pub accepted_operations: usize,
    /// Cancelled operations awaiting an exact CQE or reclamation deadline.
    pub pending_reclamations: usize,
    /// Shared-CQ admission slots currently available.
    pub available_cq_credits: usize,
    /// CQ admission debt retained by quarantined operations.
    pub retained_cq_credits: usize,
    /// Operation records retained because safe release is unproven.
    pub quarantined_operations: usize,
    /// Registered MRs retained by quarantined operations.
    pub quarantined_mrs: usize,
    /// Bytes retained by quarantined MRs.
    pub quarantined_bytes: usize,
    /// Complete connection ownership bundles retained fail-closed.
    pub quarantined_connections: usize,
}
