//! Explicitly driven, shared v2 RDMA engine.

mod config;
mod diagnostics;
mod driver;
mod resources;
mod scheduler;

#[cfg(test)]
mod api_tests;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use config::EngineConfig;
pub use config::{CompletionMode, RdmaConnectionConfig};
pub use diagnostics::{RdmaEngineDiagnostics, RdmaEngineLifecycle, RdmaEngineTerminalError};
use driver::WorkSignal;
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub use driver::{
    TestAcceptedOperation, TestConnectionIdentity, TestCqArmWindowControl, TestCqeSuppression,
    TestEngineQp, TestEngineResources, TestRouteHandle,
};
use resources::{EngineResources, ResourceSummary};
use scheduler::WorkScheduler;

use super::error::{Error, Result};

/// Builder for one device-bound, explicitly driven RDMA engine.
///
/// A kernel RDMA device name is mandatory. Readiness is the default completion
/// mode and `build()` must run inside a Tokio runtime with I/O enabled. Polling
/// mode creates no Tokio I/O registration and may be built outside a runtime.
pub struct RdmaEngineBuilder {
    config: EngineConfig,
}

impl RdmaEngineBuilder {
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            config: EngineConfig::new(device_name.into()),
        }
    }

    pub fn completion_mode(mut self, mode: CompletionMode) -> Self {
        self.config.completion_mode = mode;
        self
    }

    pub fn maximum_live_connections(mut self, value: usize) -> Self {
        self.config.max_live_connections = value;
        self
    }

    pub fn maximum_inflight_operations(mut self, value: usize) -> Self {
        self.config.max_inflight_operations = value;
        self
    }

    pub fn cq_capacity(mut self, value: usize) -> Self {
        self.config.cq_capacity = value;
        self
    }

    pub fn cq_completion_budget(mut self, value: usize) -> Self {
        self.config.cq_completion_budget = value;
        self
    }

    pub fn cm_event_budget(mut self, value: usize) -> Self {
        self.config.cm_event_budget = value;
        self
    }

    pub fn reclamation_budget(mut self, value: usize) -> Self {
        self.config.reclamation_budget = value;
        self
    }

    pub fn ready_connection_quantum(mut self, value: usize) -> Self {
        self.config.ready_connection_quantum = value;
        self
    }

    pub fn missing_cqe_deadline(mut self, value: Duration) -> Self {
        self.config.missing_cqe_deadline = value;
        self
    }

    pub fn connection_drain_deadline(mut self, value: Duration) -> Self {
        self.config.connection_drain_deadline = value;
        self
    }

    pub fn shutdown_deadline(mut self, value: Duration) -> Self {
        self.config.shutdown_deadline = value;
        self
    }

    pub fn message_hello_deadline(mut self, value: Duration) -> Self {
        self.config.hello_deadline = value;
        self
    }

    /// Allocate shared resources without starting progress.
    ///
    /// The returned driver is the engine's only progress source. Applications
    /// must poll it directly or spawn it explicitly. Readiness mode registers
    /// the shared CQ and CM descriptors with the current Tokio I/O driver;
    /// polling mode creates neither registration.
    pub fn build(self) -> Result<(RdmaEngine, RdmaEngineDriver)> {
        self.config.validate_without_provider()?;
        if self.config.completion_mode == CompletionMode::Readiness {
            preflight_tokio_io()?;
        }

        let (resources, _provider) = EngineResources::build(&self.config)?;
        let resource_summary = resources.summary();
        #[allow(unused_mut, reason = "test hooks attach safe resource references")]
        let mut shared = EngineShared::new(self.config, resource_summary);
        #[cfg(any(test, feature = "test-hooks"))]
        {
            shared.test_resources = Some(resources.test_resource_refs());
        }
        let shared = Arc::new(shared);
        let engine = RdmaEngine {
            shared: Arc::clone(&shared),
        };
        let driver = RdmaEngineDriver::new(shared, Some(resources));
        Ok((engine, driver))
    }
}

/// Cloneable frontend for one explicitly driven engine instance.
///
/// Cloning this value never starts work. All CQ, CM, reclamation, and
/// connection-local progress remains owned by the paired [`RdmaEngineDriver`].
pub struct RdmaEngine {
    shared: Arc<EngineShared>,
}

impl Clone for RdmaEngine {
    fn clone(&self) -> Self {
        self.shared.frontend_count.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl RdmaEngine {
    /// Request graceful shutdown and await the engine driver's result.
    ///
    /// Dropping this future removes its `Notify` registration, so cancelled
    /// shutdown attempts do not accumulate retained task wakers.
    pub async fn shutdown(&self) -> Result<()> {
        self.shared.request_shutdown();
        loop {
            let notified = self.shared.terminal_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.shared.outcome() {
                return outcome.into_result();
            }
            notified.await;
        }
    }

    /// Return a non-blocking snapshot of lifecycle, terminal, and capacity state.
    pub fn diagnostics(&self) -> RdmaEngineDiagnostics {
        self.shared.diagnostics()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_resources(&self) -> Result<driver::TestEngineResources> {
        let resources =
            self.shared.test_resources.clone().ok_or_else(|| {
                Error::InvalidConfig("test engine resources are unavailable".into())
            })?;
        Ok(driver::TestEngineResources::new(&self.shared, resources))
    }
}

impl Drop for RdmaEngine {
    fn drop(&mut self) {
        if self.shared.frontend_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.request_shutdown();
        }
    }
}

/// Sole progress future for an [`RdmaEngine`].
///
/// The driver performs bounded rotating service across terminal/control, CM,
/// CQ, reclamation/deadline, and ready-connection work. Readiness mode sleeps
/// only on registered event sources and published software work; polling mode
/// performs one bounded nonblocking iteration followed by a cooperative yield.
/// Dropping the driver publishes a terminal failure and wakes observed waiters.
pub struct RdmaEngineDriver {
    shared: Arc<EngineShared>,
    resources: Option<EngineResources>,
    scheduler: WorkScheduler,
    cq_readiness: crate::v2::completion::CqReadiness,
    cq_buffer: Box<[crate::wc::WorkCompletion]>,
    deadline_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    deadline_at: Option<tokio::time::Instant>,
}

struct EngineShared {
    config: EngineConfig,
    resources: ResourceSummary,
    lifecycle: AtomicU8,
    shutdown_requested: AtomicBool,
    frontend_count: AtomicUsize,
    work_signal: WorkSignal,
    // Notify stores only live Notified futures and wakes every concurrent
    // shutdown waiter without retaining registrations from dropped futures.
    terminal_notify: Notify,
    terminal: Mutex<Option<EngineOutcome>>,
    driver_yields: AtomicU64,
    #[cfg(any(test, feature = "test-hooks"))]
    test_resources: Option<resources::TestResourceRefs>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: driver::test_api::TestDriverState,
}

impl EngineShared {
    fn new(config: EngineConfig, resources: ResourceSummary) -> Self {
        Self {
            config,
            resources,
            lifecycle: AtomicU8::new(lifecycle_to_u8(RdmaEngineLifecycle::Created)),
            shutdown_requested: AtomicBool::new(false),
            frontend_count: AtomicUsize::new(1),
            work_signal: WorkSignal::new(),
            terminal_notify: Notify::new(),
            terminal: Mutex::new(None),
            driver_yields: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-hooks"))]
            test_resources: None,
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver: driver::test_api::TestDriverState::new(),
        }
    }

    fn request_shutdown(&self) {
        if !self.shutdown_requested.swap(true, Ordering::AcqRel) {
            self.transition_shutdown_requested();
        }
        self.work_signal.publish(driver::TERMINAL_WORK);
    }

    fn finish(&self, outcome: EngineOutcome) {
        let mut terminal = self
            .terminal
            .lock()
            .expect("engine terminal state poisoned");
        if terminal.is_some() {
            return;
        }
        let lifecycle = match outcome {
            EngineOutcome::Success => RdmaEngineLifecycle::Terminated,
            EngineOutcome::Failure(_) => RdmaEngineLifecycle::Failed,
        };
        *terminal = Some(outcome);
        self.transition_terminal(lifecycle);
        drop(terminal);
        self.terminal_notify.notify_waiters();
    }

    fn outcome(&self) -> Option<EngineOutcome> {
        self.terminal
            .lock()
            .expect("engine terminal state poisoned")
            .clone()
    }

    fn transition_running(&self) {
        let _ = self.lifecycle.compare_exchange(
            lifecycle_to_u8(RdmaEngineLifecycle::Created),
            lifecycle_to_u8(RdmaEngineLifecycle::Running),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn transition_shutdown_requested(&self) {
        let mut current = self.lifecycle.load(Ordering::Acquire);
        loop {
            let state = lifecycle_from_u8(current);
            if matches!(
                state,
                RdmaEngineLifecycle::ShutdownRequested
                    | RdmaEngineLifecycle::Terminated
                    | RdmaEngineLifecycle::Failed
            ) {
                return;
            }
            match self.lifecycle.compare_exchange_weak(
                current,
                lifecycle_to_u8(RdmaEngineLifecycle::ShutdownRequested),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn transition_terminal(&self, terminal: RdmaEngineLifecycle) {
        debug_assert!(matches!(
            terminal,
            RdmaEngineLifecycle::Terminated | RdmaEngineLifecycle::Failed
        ));
        let mut current = self.lifecycle.load(Ordering::Acquire);
        loop {
            if matches!(
                lifecycle_from_u8(current),
                RdmaEngineLifecycle::Terminated | RdmaEngineLifecycle::Failed
            ) {
                return;
            }
            match self.lifecycle.compare_exchange_weak(
                current,
                lifecycle_to_u8(terminal),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn lifecycle(&self) -> RdmaEngineLifecycle {
        lifecycle_from_u8(self.lifecycle.load(Ordering::Acquire))
    }

    fn diagnostics(&self) -> RdmaEngineDiagnostics {
        let outcome = self.outcome();
        RdmaEngineDiagnostics {
            lifecycle: self.lifecycle(),
            terminal_error: outcome.and_then(|outcome| outcome.summary()),
            device_name: self.config.device_name.clone(),
            completion_mode: self.config.completion_mode,
            maximum_live_connections: self.config.max_live_connections,
            maximum_inflight_operations: self.config.max_inflight_operations,
            cq_capacity: self.config.cq_capacity,
            shared_contexts: self.resources.contexts,
            shared_protection_domains: self.resources.protection_domains,
            shared_completion_queues: self.resources.completion_queues,
            shared_completion_channels: self.resources.completion_channels,
            shared_cm_event_channels: self.resources.cm_event_channels,
            driver_yields: self.driver_yields.load(Ordering::Acquire),
            #[cfg(any(test, feature = "test-hooks"))]
            accepted_test_operations: self.test_driver.accepted_outstanding(),
        }
    }
}

#[derive(Clone)]
enum EngineOutcome {
    Success,
    Failure(EngineFailure),
}

impl EngineOutcome {
    fn into_result(self) -> Result<()> {
        match self {
            Self::Success => Ok(()),
            Self::Failure(error) => Err(error.into_error()),
        }
    }

    fn summary(self) -> Option<RdmaEngineTerminalError> {
        match self {
            Self::Success => None,
            Self::Failure(error) => Some(error.summary()),
        }
    }
}

#[derive(Clone)]
enum EngineFailure {
    DriverShutdown,
    Progress(String),
    #[cfg(any(test, feature = "test-hooks"))]
    Wedged {
        retained_bundles: usize,
        outstanding_operations: usize,
    },
}

impl EngineFailure {
    fn into_error(self) -> Error {
        match self {
            Self::DriverShutdown => Error::DriverShutdown,
            Self::Progress(message) => Error::Verbs(std::io::Error::other(message)),
            #[cfg(any(test, feature = "test-hooks"))]
            Self::Wedged {
                retained_bundles,
                outstanding_operations,
            } => Error::EngineWedged {
                retained_bundles,
                outstanding_operations,
                cq_debt: outstanding_operations,
            },
        }
    }

    fn summary(self) -> RdmaEngineTerminalError {
        let error = self.into_error();
        RdmaEngineTerminalError {
            class: match error {
                Error::DriverShutdown => "DriverShutdown",
                Error::InvalidConfig(_) => "InvalidConfig",
                Error::EngineWedged { .. } => "EngineWedged",
                _ => "EngineError",
            }
            .into(),
            message: error.to_string(),
        }
    }
}

fn preflight_tokio_io() -> Result<()> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(Error::InvalidConfig(
            "readiness mode requires an active Tokio runtime with I/O enabled".into(),
        ));
    }
    Ok(())
}

const fn lifecycle_to_u8(lifecycle: RdmaEngineLifecycle) -> u8 {
    match lifecycle {
        RdmaEngineLifecycle::Created => 0,
        RdmaEngineLifecycle::Running => 1,
        RdmaEngineLifecycle::ShutdownRequested => 2,
        RdmaEngineLifecycle::Terminated => 3,
        RdmaEngineLifecycle::Failed => 4,
    }
}

const fn lifecycle_from_u8(value: u8) -> RdmaEngineLifecycle {
    match value {
        0 => RdmaEngineLifecycle::Created,
        1 => RdmaEngineLifecycle::Running,
        2 => RdmaEngineLifecycle::ShutdownRequested,
        3 => RdmaEngineLifecycle::Terminated,
        _ => RdmaEngineLifecycle::Failed,
    }
}

#[cfg(test)]
fn test_engine_pair(mode: CompletionMode) -> (RdmaEngine, RdmaEngineDriver) {
    let mut config = EngineConfig::new("test0".into());
    config.completion_mode = mode;
    let shared = Arc::new(EngineShared::new(
        config,
        ResourceSummary {
            contexts: 1,
            protection_domains: 1,
            completion_queues: 1,
            completion_channels: 0,
            cm_event_channels: 1,
        },
    ));
    (
        RdmaEngine {
            shared: Arc::clone(&shared),
        },
        RdmaEngineDriver::new(shared, None),
    )
}
