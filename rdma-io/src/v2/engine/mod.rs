//! Explicitly driven, shared v2 RDMA engine.

mod cm;
mod config;
mod connection;
mod diagnostics;
mod driver;
mod operation;
mod registry;
mod resources;
mod scheduler;

#[cfg(test)]
mod api_tests;

use std::collections::VecDeque;
#[cfg(panic = "unwind")]
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use tokio::sync::Notify;

use config::EngineConfig;
pub use config::{CompletionMode, RdmaConnectionConfig};
use connection::{ConnectionAdmissionPool, ConnectionState};
pub use connection::{RdmaConnection, RdmaConnectionIdentity};
use diagnostics::DiagnosticsState;
pub use diagnostics::{RdmaEngineDiagnostics, RdmaEngineLifecycle, RdmaEngineTerminalError};
use driver::WorkSignal;
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub use driver::{
    TestAcceptedOperation, TestConnectionIdentity, TestCqArmWindowControl, TestCqeSuppression,
    TestEngineQp, TestEngineResources, TestRouteHandle,
};
pub use operation::RdmaOperation;
use operation::{CqCreditPool, OperationRegistry};
use registry::{
    ConnectionRegistry, ConnectionToken, OperationToken, lock_unpoison, write_unpoison,
};
use resources::{EngineResourceRefs, EngineResources, ResourceSummary};
use scheduler::WorkScheduler;
use scheduler::{DeadlineKind, DeadlineRequest};

use super::error::{Error, Result};

/// Builder for one device-bound, explicitly driven RDMA engine.
///
/// A kernel RDMA device name is mandatory. Readiness is the default completion
/// mode and `build()` must run inside a Tokio runtime with I/O enabled. Polling
/// mode creates no Tokio I/O registration and may be built outside a runtime.
///
/// With `panic=abort`, the engine never probes optional Tokio capabilities by
/// deliberately triggering and catching a panic. An absent runtime is still
/// rejected with [`tokio::runtime::Handle::try_current`]. Readiness mode then
/// performs its required `AsyncFd` registrations and returns registration
/// errors reported through Tokio's fallible API. Tokio exposes no non-panicking
/// query for an active runtime whose I/O driver is disabled, so such a runtime
/// can still abort inside Tokio; callers using `panic=abort` must enable I/O.
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

        let (resources, provider) = EngineResources::build(&self.config)?;
        let resource_summary = resources.summary();
        let resource_refs = resources.connection_resource_refs();
        #[allow(unused_mut, reason = "test hooks attach safe resource references")]
        let mut shared = EngineShared::new(
            self.config,
            resource_summary,
            Some(provider),
            Some(resource_refs),
        )?;
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

    /// Establish an outbound low-level connection with the default QP/CM
    /// configuration. The engine driver owns every CM and CQ progress step.
    pub async fn connect(&self, address: std::net::SocketAddr) -> Result<RdmaConnection> {
        cm::connect(
            Arc::clone(&self.shared),
            address,
            RdmaConnectionConfig::default(),
        )
        .await
    }

    /// Establish an outbound low-level connection with an explicit validated
    /// QP/CM configuration.
    pub async fn connect_with_config(
        &self,
        address: std::net::SocketAddr,
        config: RdmaConnectionConfig,
    ) -> Result<RdmaConnection> {
        cm::connect(Arc::clone(&self.shared), address, config).await
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
///
/// With `panic=abort`, polling deliberately skips Tokio's panic-based optional
/// time-driver probe. Polling without an armed deadline therefore works on any
/// active Tokio runtime. Tokio exposes no safe time-capability query, so a
/// runtime without time enabled can still abort if later work arms a lifecycle
/// deadline; callers using those operations must enable Tokio time.
pub struct RdmaEngineDriver {
    shared: Arc<EngineShared>,
    resources: Option<EngineResources>,
    scheduler: WorkScheduler,
    cq_readiness: crate::v2::completion::CqReadiness,
    cq_buffer: Box<[crate::wc::WorkCompletion]>,
    deadline_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    deadline_at: Option<tokio::time::Instant>,
    runtime_checked: bool,
}

struct EngineShared {
    config: EngineConfig,
    resources: ResourceSummary,
    provider: Option<config::ProviderLimits>,
    resource_refs: Option<EngineResourceRefs>,
    connection_admission: Arc<ConnectionAdmissionPool>,
    connections: ConnectionRegistry,
    operations: OperationRegistry,
    cm: cm::CmState,
    cq_credits: CqCreditPool,
    diagnostic_counters: DiagnosticsState,
    accepted_operations: AtomicUsize,
    pending_reclamations: AtomicUsize,
    quarantined_operations: AtomicUsize,
    quarantined_mrs: AtomicUsize,
    quarantined_bytes: AtomicUsize,
    ready_queue_depth: AtomicUsize,
    deadline_requests: Mutex<VecDeque<DeadlineRequest>>,
    admission: RwLock<()>,
    lifecycle: AtomicU8,
    shutdown_requested: AtomicBool,
    failure_retained: AtomicBool,
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
    fn new(
        config: EngineConfig,
        resources: ResourceSummary,
        provider: Option<config::ProviderLimits>,
        resource_refs: Option<EngineResourceRefs>,
    ) -> Result<Self> {
        let connections = ConnectionRegistry::new(config.max_live_connections)?;
        let operations = OperationRegistry::new(config.max_inflight_operations)?;
        let cq_credits = CqCreditPool::new(config.cq_capacity);
        let connection_admission = ConnectionAdmissionPool::new(config.max_live_connections);
        let cm = cm::CmState::new(config.max_live_connections)?;
        Ok(Self {
            config,
            resources,
            provider,
            resource_refs,
            connection_admission,
            connections,
            operations,
            cm,
            cq_credits,
            diagnostic_counters: DiagnosticsState::default(),
            accepted_operations: AtomicUsize::new(0),
            pending_reclamations: AtomicUsize::new(0),
            quarantined_operations: AtomicUsize::new(0),
            quarantined_mrs: AtomicUsize::new(0),
            quarantined_bytes: AtomicUsize::new(0),
            ready_queue_depth: AtomicUsize::new(0),
            deadline_requests: Mutex::new(VecDeque::new()),
            admission: RwLock::new(()),
            lifecycle: AtomicU8::new(lifecycle_to_u8(RdmaEngineLifecycle::Created)),
            shutdown_requested: AtomicBool::new(false),
            failure_retained: AtomicBool::new(false),
            frontend_count: AtomicUsize::new(1),
            work_signal: WorkSignal::new(),
            terminal_notify: Notify::new(),
            terminal: Mutex::new(None),
            driver_yields: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-hooks"))]
            test_resources: None,
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver: driver::test_api::TestDriverState::new(),
        })
    }

    fn request_shutdown(&self) {
        {
            let _admission = write_unpoison(&self.admission);
            if !self.shutdown_requested.swap(true, Ordering::AcqRel) {
                self.transition_shutdown_requested();
            }
        }
        self.work_signal.publish(driver::TERMINAL_WORK);
    }

    fn finish(&self, outcome: EngineOutcome) {
        let (operations_to_wake, connections_to_wake, force_connection_error) = {
            let _admission = write_unpoison(&self.admission);
            let mut terminal = lock_unpoison(&self.terminal);
            if terminal.is_some() {
                return;
            }
            self.shutdown_requested.store(true, Ordering::Release);
            self.transition_shutdown_requested();
            let lifecycle = match &outcome {
                EngineOutcome::Success => RdmaEngineLifecycle::Terminated,
                EngineOutcome::Failure(_) => RdmaEngineLifecycle::Failed,
            };
            *terminal = Some(outcome.clone());
            self.transition_terminal(lifecycle);

            let mut operations_to_wake = Vec::new();
            if matches!(outcome, EngineOutcome::Failure(_)) {
                for operation in self.operations.occupied() {
                    let terminalized = operation.finalize_terminal(&outcome);
                    if terminalized.was_reclaiming {
                        self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
                    }
                    if terminalized.newly_quarantined {
                        self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
                        self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
                        self.quarantined_bytes
                            .fetch_add(operation.mr_len, Ordering::AcqRel);
                        self.cq_credits.retain();
                    }
                    if terminalized.should_wake {
                        operations_to_wake.push(operation);
                    }
                }
            }

            let connections_to_wake = self.connections.occupied();
            let force_error = matches!(outcome, EngineOutcome::Failure(_));
            drop(terminal);
            (operations_to_wake, connections_to_wake, force_error)
        };

        self.cm.terminalize(&outcome);
        for connection in &connections_to_wake {
            connection.finalize_engine(force_connection_error);
        }
        for operation in operations_to_wake {
            operation.wake();
        }
        for connection in connections_to_wake {
            connection.wake_close();
        }
        self.terminal_notify.notify_waiters();
    }

    fn outcome(&self) -> Option<EngineOutcome> {
        lock_unpoison(&self.terminal).clone()
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

    fn admission_error(&self) -> Option<Error> {
        if let Some(outcome) = self.outcome() {
            return outcome.into_result().err();
        }
        self.shutdown_requested
            .load(Ordering::Acquire)
            .then_some(Error::DriverShutdown)
    }

    fn retained_bundle_count(&self) -> usize {
        self.connections
            .occupied()
            .into_iter()
            .filter(|connection| connection.accepted_count() != 0)
            .count()
    }

    fn unsafe_outstanding_operations(&self) -> usize {
        let production = self.accepted_operations.load(Ordering::Acquire);
        #[cfg(any(test, feature = "test-hooks"))]
        {
            production + self.test_driver.accepted_outstanding()
        }
        #[cfg(not(any(test, feature = "test-hooks")))]
        {
            production
        }
    }

    fn retain_after_failure(shared: &Arc<Self>) {
        if (shared.unsafe_outstanding_operations() == 0 && shared.cm.retained_owner_count() == 0)
            || shared.failure_retained.swap(true, Ordering::AcqRel)
        {
            return;
        }
        failed_engine_quarantine()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::clone(shared));
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
            live_connection_reservations: self.connection_admission.used(),
            free_connection_slots: self.connections.free(),
            retired_connection_slots: self.connections.retired(),
            registered_operations: self.operations.live(),
            free_operation_slots: self.operations.free(),
            retired_operation_slots: self.operations.retired(),
            accepted_outstanding_operations: self.accepted_operations.load(Ordering::Acquire),
            free_cq_credits: self.cq_credits.free(),
            retained_cq_credits: self.cq_credits.retained(),
            pending_reclamations: self.pending_reclamations.load(Ordering::Acquire),
            quarantined_operations: self.quarantined_operations.load(Ordering::Acquire),
            quarantined_mrs: self.quarantined_mrs.load(Ordering::Acquire),
            quarantined_bytes: self.quarantined_bytes.load(Ordering::Acquire),
            ready_queue_depth: self.ready_queue_depth.load(Ordering::Acquire),
            operations_offered: self
                .diagnostic_counters
                .operations_offered
                .load(Ordering::Acquire),
            operations_accepted: self
                .diagnostic_counters
                .operations_accepted
                .load(Ordering::Acquire),
            operations_unaccepted: self
                .diagnostic_counters
                .operations_unaccepted
                .load(Ordering::Acquire),
            operations_posted: self
                .diagnostic_counters
                .operations_posted
                .load(Ordering::Acquire),
            operations_completed: self
                .diagnostic_counters
                .operations_completed
                .load(Ordering::Acquire),
            operations_cancelled: self
                .diagnostic_counters
                .operations_cancelled
                .load(Ordering::Acquire),
            batch_posts_attempted: self
                .diagnostic_counters
                .batch_posts_attempted
                .load(Ordering::Acquire),
            batch_accepted_prefix: self
                .diagnostic_counters
                .batch_accepted_prefix
                .load(Ordering::Acquire),
            batch_unaccepted_suffix: self
                .diagnostic_counters
                .batch_unaccepted_suffix
                .load(Ordering::Acquire),
            batch_ambiguous_results: self
                .diagnostic_counters
                .batch_ambiguous_results
                .load(Ordering::Acquire),
            cqes_polled: self.diagnostic_counters.cqes_polled.load(Ordering::Acquire),
            cqes_routed: self.diagnostic_counters.cqes_routed.load(Ordering::Acquire),
            stale_connection_cqes: self
                .diagnostic_counters
                .stale_connection_cqes
                .load(Ordering::Acquire),
            stale_operation_cqes: self
                .diagnostic_counters
                .stale_operation_cqes
                .load(Ordering::Acquire),
            unknown_cqes: self
                .diagnostic_counters
                .unknown_cqes
                .load(Ordering::Acquire),
            duplicate_cqes: self
                .diagnostic_counters
                .duplicate_cqes
                .load(Ordering::Acquire),
            wrong_connection_cqes: self
                .diagnostic_counters
                .wrong_connection_cqes
                .load(Ordering::Acquire),
            wrong_qp_num_cqes: self
                .diagnostic_counters
                .wrong_qp_num_cqes
                .load(Ordering::Acquire),
            unexpected_opcode_cqes: self
                .diagnostic_counters
                .unexpected_opcode_cqes
                .load(Ordering::Acquire),
            reclamation_deadlines: self
                .diagnostic_counters
                .reclamation_deadlines
                .load(Ordering::Acquire),
            connection_capacity_exhausted: self
                .diagnostic_counters
                .connection_capacity_exhausted
                .load(Ordering::Acquire),
            operation_capacity_exhausted: self
                .diagnostic_counters
                .operation_capacity_exhausted
                .load(Ordering::Acquire),
            cq_capacity_exhausted: self
                .diagnostic_counters
                .cq_capacity_exhausted
                .load(Ordering::Acquire),
            connections_opened: self
                .diagnostic_counters
                .connections_opened
                .load(Ordering::Acquire),
            connections_failed: self
                .diagnostic_counters
                .connections_failed
                .load(Ordering::Acquire),
            cm_events_processed: self
                .diagnostic_counters
                .cm_events_processed
                .load(Ordering::Acquire),
            cm_events_rejected: self
                .diagnostic_counters
                .cm_events_rejected
                .load(Ordering::Acquire),
            stale_cm_events: self
                .diagnostic_counters
                .stale_cm_events
                .load(Ordering::Acquire),
            duplicate_cm_events: self
                .diagnostic_counters
                .duplicate_cm_events
                .load(Ordering::Acquire),
            unknown_cm_events: self
                .diagnostic_counters
                .unknown_cm_events
                .load(Ordering::Acquire),
            wrong_id_cm_events: self
                .diagnostic_counters
                .wrong_id_cm_events
                .load(Ordering::Acquire),
            unexpected_cm_events: self
                .diagnostic_counters
                .unexpected_cm_events
                .load(Ordering::Acquire),
            #[cfg(any(test, feature = "test-hooks"))]
            accepted_test_operations: self.test_driver.accepted_outstanding(),
        }
    }

    fn register_memory(&self, len: usize, access: super::AccessIntent) -> Result<super::Mr> {
        if len == 0 || u32::try_from(len).is_err() {
            return Err(Error::InvalidConfig(
                "engine MR length must be in 1..=u32::MAX".into(),
            ));
        }
        let resources = self.resource_refs.as_ref().ok_or_else(|| {
            Error::InvalidConfig("engine shared protection domain is unavailable".into())
        })?;
        resources.pd.reg_mr(len, access)
    }

    fn schedule_reclamation(&self, token: OperationToken) {
        self.begin_reclamation(token);
        self.schedule_deadline(
            DeadlineKind::Reclamation,
            token.encode(),
            self.config.missing_cqe_deadline,
        );
    }

    fn begin_connection_close(&self, connection: &Arc<ConnectionState>, force_error: bool) {
        if connection.is_retired() {
            return;
        }
        let first = connection.begin_close();
        if force_error {
            let _ = connection.transition_to_error_once();
        }
        if first {
            self.schedule_connection_drain(connection.token);
        }
        if connection.accepted_count() == 0 {
            self.schedule_connection_retirement(connection);
        }
    }

    fn schedule_connection_retirement(&self, connection: &ConnectionState) {
        if connection.is_retired() || !connection.try_request_retirement() {
            return;
        }
        self.cm.enqueue_retirement(connection.token);
        self.work_signal.publish(cm::CM_WORK);
    }

    fn schedule_connection_drain(&self, token: ConnectionToken) {
        self.schedule_deadline(
            DeadlineKind::ConnectionDrain,
            token.encode(),
            self.config.connection_drain_deadline,
        );
    }

    fn schedule_deadline(&self, kind: DeadlineKind, token: u64, after: Duration) {
        let now = tokio::time::Instant::now();
        let at = now.checked_add(after).unwrap_or(now);
        lock_unpoison(&self.deadline_requests).push_back(DeadlineRequest { at, kind, token });
        self.work_signal.publish(driver::RECLAMATION_WORK);
    }

    fn take_deadline_requests(&self, budget: usize) -> Vec<DeadlineRequest> {
        let mut requests = lock_unpoison(&self.deadline_requests);
        let count = requests.len().min(budget);
        requests.drain(..count).collect()
    }

    fn has_deadline_requests(&self) -> bool {
        !lock_unpoison(&self.deadline_requests).is_empty()
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

#[cfg(panic = "unwind")]
pub(super) enum RuntimeProbe<T> {
    Completed(T),
    Panicked,
}

#[cfg(panic = "unwind")]
pub(super) fn probe_runtime<T>(probe: impl FnOnce() -> T) -> RuntimeProbe<T> {
    // Tokio exposes no capability query for optional I/O/time drivers. Serialize
    // the constructor probe and suppress only its current-thread panic; panics
    // from every other thread still reach the application's installed hook.
    static PROBE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _probe = PROBE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let thread = std::thread::current().id();
    type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;
    let previous: Arc<Mutex<Option<PanicHook>>> =
        Arc::new(Mutex::new(Some(std::panic::take_hook())));
    let fallback = Arc::clone(&previous);
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() != thread {
            let hook = fallback.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(hook) = hook.as_ref() {
                hook(info);
            }
        }
    }));
    let result = catch_unwind(AssertUnwindSafe(probe));
    let previous = previous
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .expect("runtime probe panic hook");
    std::panic::set_hook(previous);
    match result {
        Ok(value) => RuntimeProbe::Completed(value),
        Err(_) => RuntimeProbe::Panicked,
    }
}

fn failed_engine_quarantine() -> &'static Mutex<Vec<Arc<EngineShared>>> {
    // No progress source remains after terminal driver failure, so accepted
    // QP/CM/MR bundles cannot reach a positive release boundary.
    static ENGINES: OnceLock<Mutex<Vec<Arc<EngineShared>>>> = OnceLock::new();
    ENGINES.get_or_init(|| Mutex::new(Vec::new()))
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
    let shared = Arc::new(
        EngineShared::new(
            config,
            ResourceSummary {
                contexts: 1,
                protection_domains: 1,
                completion_queues: 1,
                completion_channels: 0,
                cm_event_channels: 1,
            },
            None,
            None,
        )
        .unwrap(),
    );
    (
        RdmaEngine {
            shared: Arc::clone(&shared),
        },
        RdmaEngineDriver::new(shared, None),
    )
}
