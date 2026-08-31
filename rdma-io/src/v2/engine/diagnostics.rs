use super::config::CompletionMode;

/// Public engine lifecycle state included in diagnostics snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdmaEngineLifecycle {
    Created,
    Running,
    ShutdownRequested,
    Terminated,
    Failed,
}

/// Non-owning terminal error summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RdmaEngineTerminalError {
    pub class: String,
    pub message: String,
}

/// Concurrently readable snapshot of engine state and configured capacity.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RdmaEngineDiagnostics {
    pub lifecycle: RdmaEngineLifecycle,
    pub terminal_error: Option<RdmaEngineTerminalError>,
    pub device_name: String,
    pub completion_mode: CompletionMode,
    pub maximum_live_connections: usize,
    pub maximum_inflight_operations: usize,
    pub cq_capacity: usize,
    pub shared_contexts: usize,
    pub shared_protection_domains: usize,
    pub shared_completion_queues: usize,
    pub shared_completion_channels: usize,
    pub shared_cm_event_channels: usize,
}
