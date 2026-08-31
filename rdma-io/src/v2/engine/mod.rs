//! Explicitly driven, shared v2 RDMA engine.

mod config;
mod diagnostics;
mod resources;

#[cfg(test)]
mod api_tests;

use std::future::Future;
use std::os::fd::{FromRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::Duration;

pub use config::{CompletionMode, RdmaConnectionConfig};
pub use diagnostics::{RdmaEngineDiagnostics, RdmaEngineLifecycle, RdmaEngineTerminalError};
use tokio::io::unix::AsyncFd;

use config::EngineConfig;
use resources::{EngineResources, ResourceSummary};

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
    pub fn build(self) -> Result<(RdmaEngine, RdmaEngineDriver)> {
        self.config.validate_without_provider()?;
        if self.config.completion_mode == CompletionMode::Readiness {
            preflight_tokio_io()?;
        }

        let (resources, _provider) = EngineResources::build(&self.config)?;
        let resource_summary = resources.summary();
        let shared = Arc::new(EngineShared::new(self.config, resource_summary));
        let engine = RdmaEngine {
            shared: Arc::clone(&shared),
        };
        let driver = RdmaEngineDriver {
            shared,
            resources: Some(resources),
            time_context_checked: false,
        };
        Ok((engine, driver))
    }
}

/// Cloneable frontend for one engine instance.
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
    pub async fn shutdown(&self) -> Result<()> {
        self.shared.request_shutdown();
        std::future::poll_fn(|cx| {
            if let Some(outcome) = self.shared.outcome() {
                return Poll::Ready(outcome.into_result());
            }
            self.shared.register_terminal_waiter(cx.waker());
            match self.shared.outcome() {
                Some(outcome) => Poll::Ready(outcome.into_result()),
                None => Poll::Pending,
            }
        })
        .await
    }

    /// Return a non-blocking snapshot of lifecycle, terminal, and capacity state.
    pub fn diagnostics(&self) -> RdmaEngineDiagnostics {
        self.shared.diagnostics()
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
pub struct RdmaEngineDriver {
    shared: Arc<EngineShared>,
    resources: Option<EngineResources>,
    time_context_checked: bool,
}

impl Future for RdmaEngineDriver {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if let Some(outcome) = self.shared.outcome() {
            self.resources.take();
            return Poll::Ready(outcome.into_result());
        }

        if !self.time_context_checked {
            if let Err(error) = preflight_tokio_time() {
                let outcome = EngineOutcome::Failure(EngineFailure::InvalidRuntime(error));
                self.shared.finish(outcome.clone());
                self.resources.take();
                return Poll::Ready(outcome.into_result());
            }
            self.time_context_checked = true;
            self.shared.set_lifecycle(RdmaEngineLifecycle::Running);
        }

        if self.shared.shutdown_requested.load(Ordering::Acquire) {
            let outcome = EngineOutcome::Success;
            self.shared.finish(outcome.clone());
            self.resources.take();
            return Poll::Ready(outcome.into_result());
        }

        self.shared.register_driver_waker(cx.waker());
        if self.shared.shutdown_requested.load(Ordering::Acquire) {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

impl Drop for RdmaEngineDriver {
    fn drop(&mut self) {
        if self.shared.outcome().is_none() {
            self.shared
                .finish(EngineOutcome::Failure(EngineFailure::DriverShutdown));
        }
    }
}

struct EngineShared {
    config: EngineConfig,
    resources: ResourceSummary,
    lifecycle: AtomicU8,
    shutdown_requested: AtomicBool,
    frontend_count: AtomicUsize,
    driver_waker: Mutex<Option<Waker>>,
    terminal_waiters: Mutex<Vec<Waker>>,
    terminal: Mutex<Option<EngineOutcome>>,
}

impl EngineShared {
    fn new(config: EngineConfig, resources: ResourceSummary) -> Self {
        Self {
            config,
            resources,
            lifecycle: AtomicU8::new(lifecycle_to_u8(RdmaEngineLifecycle::Created)),
            shutdown_requested: AtomicBool::new(false),
            frontend_count: AtomicUsize::new(1),
            driver_waker: Mutex::new(None),
            terminal_waiters: Mutex::new(Vec::new()),
            terminal: Mutex::new(None),
        }
    }

    fn request_shutdown(&self) {
        if !self.shutdown_requested.swap(true, Ordering::AcqRel) {
            self.set_lifecycle(RdmaEngineLifecycle::ShutdownRequested);
        }
        if let Some(waker) = self
            .driver_waker
            .lock()
            .expect("engine driver waker poisoned")
            .take()
        {
            waker.wake();
        }
    }

    fn register_driver_waker(&self, waker: &Waker) {
        let mut slot = self
            .driver_waker
            .lock()
            .expect("engine driver waker poisoned");
        if slot
            .as_ref()
            .is_none_or(|registered| !registered.will_wake(waker))
        {
            *slot = Some(waker.clone());
        }
    }

    fn register_terminal_waiter(&self, waker: &Waker) {
        let mut waiters = self
            .terminal_waiters
            .lock()
            .expect("engine terminal waiter list poisoned");
        if waiters
            .iter()
            .all(|registered| !registered.will_wake(waker))
        {
            waiters.push(waker.clone());
        }
    }

    fn finish(&self, outcome: EngineOutcome) {
        let mut terminal = self
            .terminal
            .lock()
            .expect("engine terminal state poisoned");
        if terminal.is_some() {
            return;
        }
        self.set_lifecycle(match outcome {
            EngineOutcome::Success => RdmaEngineLifecycle::Terminated,
            EngineOutcome::Failure(_) => RdmaEngineLifecycle::Failed,
        });
        *terminal = Some(outcome);
        drop(terminal);
        for waiter in self
            .terminal_waiters
            .lock()
            .expect("engine terminal waiter list poisoned")
            .drain(..)
        {
            waiter.wake();
        }
    }

    fn outcome(&self) -> Option<EngineOutcome> {
        self.terminal
            .lock()
            .expect("engine terminal state poisoned")
            .clone()
    }

    fn set_lifecycle(&self, lifecycle: RdmaEngineLifecycle) {
        self.lifecycle
            .store(lifecycle_to_u8(lifecycle), Ordering::Release);
    }

    fn diagnostics(&self) -> RdmaEngineDiagnostics {
        let outcome = self.outcome();
        RdmaEngineDiagnostics {
            lifecycle: lifecycle_from_u8(self.lifecycle.load(Ordering::Acquire)),
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
    InvalidRuntime(String),
}

impl EngineFailure {
    fn into_error(self) -> Error {
        match self {
            Self::DriverShutdown => Error::DriverShutdown,
            Self::InvalidRuntime(message) => Error::InvalidConfig(message),
        }
    }

    fn summary(self) -> RdmaEngineTerminalError {
        let error = self.into_error();
        RdmaEngineTerminalError {
            class: match error {
                Error::DriverShutdown => "DriverShutdown",
                Error::InvalidConfig(_) => "InvalidConfig",
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

    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(Error::Verbs(std::io::Error::last_os_error()));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    match catch_unwind(AssertUnwindSafe(|| AsyncFd::new(owned))) {
        Ok(Ok(_probe)) => Ok(()),
        Ok(Err(error)) => Err(Error::InvalidConfig(format!(
            "readiness mode requires Tokio I/O support: {error}"
        ))),
        Err(_) => Err(Error::InvalidConfig(
            "readiness mode requires an active Tokio runtime with I/O enabled".into(),
        )),
    }
}

fn preflight_tokio_time() -> std::result::Result<(), String> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err("engine driver must be polled inside an active Tokio runtime".into());
    }
    match catch_unwind(AssertUnwindSafe(|| tokio::time::sleep(Duration::ZERO))) {
        Ok(_sleep) => Ok(()),
        Err(_) => Err("engine driver requires Tokio time support".into()),
    }
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
fn test_engine_pair() -> (RdmaEngine, RdmaEngineDriver) {
    let config = EngineConfig::new("test0".into());
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
        RdmaEngineDriver {
            shared,
            resources: None,
            time_context_checked: false,
        },
    )
}
