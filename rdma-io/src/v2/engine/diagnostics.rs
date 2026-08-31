use super::config::CompletionMode;

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
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub accepted_test_operations: usize,
}
