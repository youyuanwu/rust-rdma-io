//! Explicitly driven, shared v2 RDMA engine.
//!
//! One engine owns one anchored verbs context facade, one protection domain,
//! one send/receive CQ, one CM event channel, and mode-specific CQ
//! notification resources. Readiness owns one completion channel/fd; polling
//! owns none. Every connection shares those objects.
//!
//! The driver routes a CQE only when the current connection generation,
//! operation generation, operation owner, and provider-reported `qp_num` all
//! agree. It is also the sole CM event consumer. Cancellation, close, shutdown,
//! and driver loss retain accepted or acceptance-ambiguous MRs until an exact
//! completion or provider-proven rejection establishes a positive safety
//! boundary.
//!
//! ```no_run
//! # use rdma_io::v2::{RdmaEngineBuilder, Result};
//! # async fn run_engine() -> Result<()> {
//! let (engine, driver) = RdmaEngineBuilder::new("rxe0").build()?;
//! let driver_task = tokio::spawn(driver);
//!
//! // All connections and listeners created through `engine` share this driver.
//! engine.shutdown().await?;
//! driver_task.await.expect("engine driver task panicked")?;
//! # Ok(())
//! # }
//! ```
//!
//! See the crate repository's `docs/design/v2-rdma-engine.md` for the complete
//! architecture, configuration table, wakeup proof, and provider procedure.

mod cm;
mod config;
mod connection;
mod diagnostics;
mod drain;
mod driver;
mod lifecycle;
mod listener;
mod operation;
mod registry;
mod resources;
mod scheduler;

#[cfg(test)]
mod api_tests;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
#[cfg(panic = "unwind")]
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use config::EngineConfig;
pub use config::{CompletionMode, RdmaConnectionConfig};
#[cfg(test)]
pub(crate) use config::{
    DEFAULT_MESSAGE_HELLO_DEADLINE, MAX_MESSAGE_HELLO_DEADLINE, MIN_MESSAGE_HELLO_DEADLINE,
};
use connection::ConnectionAdmissionPool;
pub(crate) use connection::ConnectionState;
pub use connection::{RdmaConnection, RdmaConnectionIdentity};
use diagnostics::DiagnosticsState;
pub use diagnostics::{
    RdmaConnectionDiagnostics, RdmaEngineDiagnostics, RdmaEngineLifecycle, RdmaEngineTerminalError,
    RdmaListenerDiagnostics,
};
use driver::WorkSignal;
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub use driver::{
    TestAcceptedOperation, TestAdmissionBarrier, TestConnectionCqeSuppression, TestContextIdentity,
    TestCqArmWindowControl, TestCqeSuppression, TestEngineInstrumentation, TestEngineQp,
    TestEngineResources, TestProviderLimits, TestReadyWorkControl, TestRouteHandle,
};
use lifecycle::MemoizedTerminalResult;
pub use listener::{RdmaListener, RdmaListenerConfig};
pub(crate) use operation::DetachedOperationCompletion;
pub use operation::RdmaOperation;
#[cfg(test)]
pub(crate) use operation::{BatchOwnershipTransfer, PreparedBatchOwnership};
use operation::{CqCreditPool, OperationRegistry};
use registry::{ConnectionRegistry, OperationToken, lock_unpoison, write_unpoison};
use resources::{EngineResourceRefs, EngineResources, ResourceSummary};
use scheduler::WorkScheduler;
use scheduler::{DeadlineKind, DeadlineRequest};

use super::error::{Error, Result};

pub(crate) trait PreEstablishSetup: Send {
    fn run(self: Box<Self>, connection: &RdmaConnection) -> Result<SetupSummary>;
}

pub(crate) trait ConnectionReadyWork: Send + Sync {
    fn process(&self, budget: usize) -> usize;
    fn has_work(&self) -> bool;
    fn deadline_expired(&self);
    fn disconnected(&self);
    fn terminalize(&self, error: Error);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SetupSummary {
    pub(crate) posted_wrs: usize,
}

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
///
/// # Use case
///
/// Construct one device-bound engine and its sole explicit driver future.
///
/// # Ownership and progress
///
/// `build` returns the handle and driver without spawning either.
///
/// # Safety and limits
///
/// Configuration, provider limits, registry layouts, and checked arithmetic
/// are validated before dependent resources escape.
///
/// # Availability
///
/// Available with the `tokio` feature.
pub struct RdmaEngineBuilder {
    config: EngineConfig,
}

impl RdmaEngineBuilder {
    /// Select the exact kernel RDMA device used by this engine.
    ///
    /// The name must be non-empty and must identify a context returned by
    /// librdmacm. Every routed CM ID must later report the same raw context
    /// pointer before the engine creates a QP.
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            config: EngineConfig::new(device_name.into()),
        }
    }

    /// Select readiness (default) or direct shared-CQ polling.
    pub fn completion_mode(mut self, mode: CompletionMode) -> Self {
        self.config.completion_mode = mode;
        self
    }

    /// Set aggregate connection admission in `1..=1_048_576`.
    pub fn maximum_live_connections(mut self, value: usize) -> Self {
        self.config.max_live_connections = value;
        self
    }

    /// Set global operation registrations in `2..=16_777_216`.
    pub fn maximum_inflight_operations(mut self, value: usize) -> Self {
        self.config.max_inflight_operations = value;
        self
    }

    /// Set shared-CQ capacity in `2..=16_777_216`.
    ///
    /// This must be at least the maximum in-flight operation count and no
    /// greater than the selected provider's `max_cqe`.
    pub fn cq_capacity(mut self, value: usize) -> Self {
        self.config.cq_capacity = value;
        self
    }

    /// Set CQ completions handled per CQ service turn in `1..=4096`.
    pub fn cq_completion_budget(mut self, value: usize) -> Self {
        self.config.cq_completion_budget = value;
        self
    }

    /// Set CM actions handled per CM service turn in `1..=4096`.
    pub fn cm_event_budget(mut self, value: usize) -> Self {
        self.config.cm_event_budget = value;
        self
    }

    /// Set reclamation/deadline actions per service turn in `1..=4096`.
    pub fn reclamation_budget(mut self, value: usize) -> Self {
        self.config.reclamation_budget = value;
        self
    }

    /// Set connection-local work before tail rotation in `1..=4096`.
    pub fn ready_connection_quantum(mut self, value: usize) -> Self {
        self.config.ready_connection_quantum = value;
        self
    }

    /// Set the cancellation reclamation deadline in `1 second..=24 hours`.
    pub fn missing_cqe_deadline(mut self, value: Duration) -> Self {
        self.config.missing_cqe_deadline = value;
        self
    }

    /// Set the connection drain deadline in `1 millisecond..=5 minutes`.
    pub fn connection_drain_deadline(mut self, value: Duration) -> Self {
        self.config.connection_drain_deadline = value;
        self
    }

    /// Set the engine shutdown deadline in `1 millisecond..=10 minutes`.
    pub fn shutdown_deadline(mut self, value: Duration) -> Self {
        self.config.shutdown_deadline = value;
        self
    }

    /// Set the message HELLO deadline in `1 millisecond..=5 minutes`.
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
        let shared = EngineShared::new(
            self.config,
            resource_summary,
            Some(provider),
            Some(resource_refs),
        )?;
        #[cfg(any(test, feature = "test-hooks"))]
        let shared = {
            let mut shared = shared;
            shared.test_resources = Some(resources.test_resource_refs());
            shared
        };
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
/// The handle is `Clone + Send + Sync + 'static`.
///
/// Dropping the last `RdmaEngine` handle requests engine shutdown. Existing
/// [`RdmaConnection`], [`RdmaListener`], and message-transport handles retain
/// shared safety state but do not count as engine frontend handles and do not
/// prevent that shutdown request. Keep at least one engine clone alive until
/// new submissions are finished, and prefer [`RdmaEngine::shutdown`] when the
/// terminal result must be observed.
pub struct RdmaEngine {
    pub(crate) shared: Arc<EngineShared>,
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
    /// shutdown attempts do not accumulate retained task wakers. The default
    /// deadline is 30 seconds; unresolved accepted WR bundles return
    /// [`Error::EngineWedged`] and remain retained fail-closed.
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

    /// Return an O(1), non-blocking aggregate snapshot.
    ///
    /// The snapshot remains readable after terminal state. Per-connection and
    /// per-listener detail is collected only through
    /// [`RdmaEngineDiagnostics::connections`] and
    /// [`RdmaEngineDiagnostics::listeners`].
    pub fn diagnostics(&self) -> RdmaEngineDiagnostics {
        self.shared.diagnostics()
    }

    /// Establish an outbound low-level connection with the default QP/CM
    /// configuration. The engine driver owns every CM and CQ progress step.
    ///
    /// Low-level establishment posts zero initial receives. With the default
    /// infinite RNR retry, a peer's early send can wait until the application
    /// posts a receive.
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
    ///
    /// Validation and aggregate admission complete before QP creation or WR
    /// posting. No requested value is silently clamped.
    pub async fn connect_with_config(
        &self,
        address: std::net::SocketAddr,
        config: RdmaConnectionConfig,
    ) -> Result<RdmaConnection> {
        cm::connect(Arc::clone(&self.shared), address, config).await
    }

    pub(crate) async fn connect_with_setup(
        &self,
        address: std::net::SocketAddr,
        config: RdmaConnectionConfig,
        setup: Box<dyn PreEstablishSetup>,
    ) -> Result<RdmaConnection> {
        cm::connect_with_setup(Arc::clone(&self.shared), address, config, setup).await
    }

    pub(crate) fn validate_message_connection_config(
        &self,
        config: &RdmaConnectionConfig,
    ) -> Result<()> {
        config.validate(&self.shared.config, self.shared.provider.as_ref())
    }

    /// Bind an engine-owned listener on the shared CM event channel.
    ///
    /// `config.backlog_capacity()` is the userspace pending-child queue limit
    /// and must be in `1..=4096`. Independently, the engine requests
    /// `i32::MAX` from the kernel through `rdma_listen`. Providers may clamp
    /// that kernel request, reducing how many requests reach userspace, or
    /// refuse it. Refusal is returned here as a contextual listener-creation
    /// error and is not counted as a userspace `BacklogFull` rejection.
    pub async fn listen(
        &self,
        address: std::net::SocketAddr,
        config: RdmaListenerConfig,
    ) -> Result<RdmaListener> {
        listener::listen(Arc::clone(&self.shared), address, config).await
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
/// Drop performs one bounded pass over registered connections, with at most
/// one QP ERR transition and one zero-outstanding QP destroy attempt per
/// connection. Individual verbs/librdmacm destructors have no wall-clock
/// guarantee, so latency-sensitive runtimes should await graceful shutdown
/// before dropping or aborting the driver task.
///
/// Ordinary polls can also execute synchronous FFI. Depending on the selected
/// work, a poll may poll/arm/get/ack CQ events; create, bind, listen, resolve,
/// connect, accept, reject, disconnect, or destroy CM IDs; create/modify/post
/// SEND or RECV work to/destroy QPs; and register or deregister MRs. These
/// provider calls have no wall-clock latency guarantee; run the driver where
/// occasional blocking provider work cannot stall unrelated latency-sensitive
/// futures.
///
/// With `panic=abort`, polling deliberately skips Tokio's panic-based optional
/// time-driver probe. Polling without an armed deadline therefore works on any
/// active Tokio runtime. Tokio exposes no safe time-capability query, so a
/// runtime without time enabled can still abort if later work arms a lifecycle
/// deadline; callers using those operations must enable Tokio time.
///
/// The future output is `rdma_io::v2::Result<()>`; it can be passed directly
/// to `tokio::spawn` without a wrapper method. Exactly one driver is returned
/// per successful build, and the engine creates zero internal tasks.
pub struct RdmaEngineDriver {
    shared: Arc<EngineShared>,
    resources: Option<EngineResources>,
    scheduler: WorkScheduler,
    cq_readiness: crate::v2::completion::CqReadiness,
    cq_buffer: Box<[super::Completion]>,
    deadline_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    deadline_at: Option<tokio::time::Instant>,
    runtime_checked: bool,
}

pub(crate) struct EngineShared {
    config: EngineConfig,
    resources: ResourceSummary,
    provider: Option<config::ProviderLimits>,
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
    published_ready_connections: Mutex<VecDeque<registry::ConnectionToken>>,
    deadline_requests: Mutex<VecDeque<DeadlineRequest>>,
    admission: RwLock<()>,
    lifecycle: AtomicU8,
    shutdown_requested: AtomicBool,
    shutdown_deadline_scheduled: AtomicBool,
    shutdown_connection_close_started: AtomicBool,
    failure_retained: AtomicBool,
    frontend_count: AtomicUsize,
    work_signal: WorkSignal,
    // Notify stores only live Notified futures and wakes every concurrent
    // shutdown waiter without retaining registrations from dropped futures.
    terminal_notify: Notify,
    terminal: Mutex<Option<MemoizedTerminalResult>>,
    driver_yields: AtomicU64,
    quarantines: Mutex<QuarantineState>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_resources: Option<resources::TestResourceRefs>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: driver::test_api::TestDriverState,
    // Rust drops fields in declaration order. Keep this root retain after every
    // registry/test owner so quarantined QP/CM/MR descendants are released
    // before the shared CQ, PD, CM event channel, and context can disappear.
    resource_refs: Option<EngineResourceRefs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum QuarantineKey {
    Connection(registry::ConnectionToken),
    Operation(registry::OperationToken),
}

#[derive(Clone, Copy)]
struct QuarantineEntry {
    connection: registry::ConnectionToken,
    started: Instant,
}

#[derive(Default)]
struct QuarantineState {
    entries: HashMap<QuarantineKey, QuarantineEntry>,
    starts: BTreeMap<Instant, usize>,
    oldest: Option<Instant>,
    connection_entries: HashMap<registry::ConnectionToken, usize>,
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
            published_ready_connections: Mutex::new(VecDeque::new()),
            deadline_requests: Mutex::new(VecDeque::new()),
            admission: RwLock::new(()),
            lifecycle: AtomicU8::new(lifecycle_to_u8(RdmaEngineLifecycle::Created)),
            shutdown_requested: AtomicBool::new(false),
            shutdown_deadline_scheduled: AtomicBool::new(false),
            shutdown_connection_close_started: AtomicBool::new(false),
            failure_retained: AtomicBool::new(false),
            frontend_count: AtomicUsize::new(1),
            work_signal: WorkSignal::new(),
            terminal_notify: Notify::new(),
            terminal: Mutex::new(None),
            driver_yields: AtomicU64::new(0),
            quarantines: Mutex::new(QuarantineState::default()),
            #[cfg(any(test, feature = "test-hooks"))]
            test_resources: None,
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver: driver::test_api::TestDriverState::new(),
            resource_refs,
        })
    }

    fn request_shutdown(&self) {
        #[cfg(any(test, feature = "test-hooks"))]
        self.test_driver.record_shutdown_attempt();
        if self.mark_shutdown_requested() {
            self.diagnostic_counters
                .shutdowns
                .fetch_add(1, Ordering::Relaxed);
        }
        if !self
            .shutdown_deadline_scheduled
            .swap(true, Ordering::AcqRel)
        {
            self.schedule_deadline(
                DeadlineKind::EngineShutdown,
                0,
                self.config.shutdown_deadline,
            );
        }
        self.work_signal.publish(driver::TERMINAL_WORK);
    }

    fn mark_shutdown_requested(&self) -> bool {
        let _admission = write_unpoison(&self.admission);
        if self.shutdown_requested.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.transition_shutdown_requested();
        true
    }

    fn finish(&self, outcome: MemoizedTerminalResult) {
        assert!(
            !outcome.is_connection_quarantined(),
            "ConnectionQuarantined is connection-local and cannot terminate the engine driver"
        );
        let (operations_to_wake, connections_to_wake) = {
            let _admission = write_unpoison(&self.admission);
            let mut terminal = lock_unpoison(&self.terminal);
            if terminal.is_some() {
                return;
            }
            self.shutdown_requested.store(true, Ordering::Release);
            self.transition_shutdown_requested();
            let lifecycle = if outcome.is_success() {
                RdmaEngineLifecycle::Terminated
            } else {
                self.diagnostic_counters
                    .terminal_driver_errors
                    .fetch_add(1, Ordering::Relaxed);
                RdmaEngineLifecycle::Failed
            };
            *terminal = Some(outcome.clone());
            self.transition_terminal(lifecycle);

            let mut operations_to_wake = Vec::new();
            if outcome.is_error() {
                for operation in self.operations.occupied() {
                    let terminalized = operation.finalize_terminal(&outcome);
                    debug_assert!(
                        !terminalized.was_reclaiming || terminalized.newly_quarantined,
                        "terminal reclamation must transfer its retained MR and CQ debt to quarantine"
                    );
                    if terminalized.was_reclaiming {
                        self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
                    }
                    if terminalized.newly_quarantined {
                        self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
                        self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
                        self.quarantined_bytes
                            .fetch_add(operation.mr_len, Ordering::AcqRel);
                        self.cq_credits.retain();
                        self.diagnostic_counters
                            .cq_credits_retained
                            .fetch_add(1, Ordering::Relaxed);
                        if self.track_operation_quarantine(&operation) {
                            self.diagnostic_counters
                                .connections_quarantined
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if terminalized.should_wake {
                        operations_to_wake.push(operation);
                    }
                }
            }

            let connections_to_wake = self.connections.occupied();
            drop(terminal);
            (operations_to_wake, connections_to_wake)
        };

        self.cm.terminalize(&outcome);
        for connection in &connections_to_wake {
            if outcome.is_error()
                && connection.retain_bundle_for_engine_failure()
                && self.track_connection_quarantine(connection.token)
            {
                self.diagnostic_counters
                    .connections_quarantined
                    .fetch_add(1, Ordering::Relaxed);
            }
            connection.finalize_engine(&outcome);
        }
        for operation in operations_to_wake {
            operation.wake();
        }
        for connection in connections_to_wake {
            connection.wake_close();
        }
        self.terminal_notify.notify_waiters();
    }

    fn outcome(&self) -> Option<MemoizedTerminalResult> {
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
        self.connections.live().max(self.cm.retained_owner_count())
    }

    fn unsafe_outstanding_operations(&self) -> usize {
        self.accepted_operations.load(Ordering::Acquire)
    }

    fn retain_after_failure(shared: &Arc<Self>) {
        if (shared.unsafe_outstanding_operations() == 0
            && shared.connections.live() == 0
            && shared.cm.retained_owner_count() == 0)
            || shared.failure_retained.swap(true, Ordering::AcqRel)
        {
            return;
        }
        failed_engine_quarantine()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::clone(shared));
    }

    fn shared_driver_wakeups(&self) -> u64 {
        self.work_signal.wakeups()
    }

    fn track_connection_quarantine(&self, token: registry::ConnectionToken) -> bool {
        self.track_quarantine(
            QuarantineKey::Connection(token),
            QuarantineEntry {
                connection: token,
                started: Instant::now(),
            },
        )
    }

    fn track_operation_quarantine(&self, operation: &operation::OperationState) -> bool {
        self.track_quarantine(
            QuarantineKey::Operation(operation.token()),
            QuarantineEntry {
                connection: operation.connection_token(),
                started: Instant::now(),
            },
        )
    }

    fn track_quarantine(&self, key: QuarantineKey, entry: QuarantineEntry) -> bool {
        let mut quarantines = lock_unpoison(&self.quarantines);
        if quarantines.entries.contains_key(&key) {
            return false;
        }
        quarantines.entries.insert(key, entry);
        *quarantines.starts.entry(entry.started).or_insert(0) += 1;
        if quarantines
            .oldest
            .is_none_or(|oldest| entry.started < oldest)
        {
            quarantines.oldest = Some(entry.started);
        }
        let connection_entries = quarantines
            .connection_entries
            .entry(entry.connection)
            .or_insert(0);
        let first_for_connection = *connection_entries == 0;
        *connection_entries += 1;
        if first_for_connection
            && let registry::Lookup::Occupied(connection) =
                self.connections.lookup(entry.connection)
        {
            connection.mark_diagnostic_quarantined();
        }
        first_for_connection
    }

    fn clear_connection_quarantine(&self, token: registry::ConnectionToken) -> bool {
        self.clear_quarantine(QuarantineKey::Connection(token), token, false)
    }

    fn recover_connection_quarantine_entry(&self, token: registry::ConnectionToken) -> bool {
        self.clear_quarantine(QuarantineKey::Connection(token), token, true)
    }

    fn clear_operation_quarantine(&self, operation: &operation::OperationState) -> bool {
        self.clear_quarantine(
            QuarantineKey::Operation(operation.token()),
            operation.connection_token(),
            true,
        )
    }

    fn clear_quarantine(
        &self,
        key: QuarantineKey,
        connection: registry::ConnectionToken,
        record_recovery: bool,
    ) -> bool {
        let mut quarantines = lock_unpoison(&self.quarantines);
        let Some(entry) = quarantines.entries.remove(&key) else {
            return false;
        };
        if let Some(count) = quarantines.starts.get_mut(&entry.started) {
            *count -= 1;
            if *count == 0 {
                quarantines.starts.remove(&entry.started);
                if quarantines.oldest == Some(entry.started) {
                    quarantines.oldest =
                        quarantines.starts.first_key_value().map(|(time, _)| *time);
                }
            }
        }
        let Some(connection_entries) = quarantines.connection_entries.get_mut(&connection) else {
            debug_assert!(false, "quarantine entry must have a connection count");
            return false;
        };
        *connection_entries -= 1;
        if *connection_entries != 0 {
            return false;
        }
        if record_recovery {
            self.diagnostic_counters
                .quarantine_recoveries
                .fetch_add(1, Ordering::Relaxed);
        }
        if let registry::Lookup::Occupied(connection) = self.connections.lookup(connection) {
            connection.mark_diagnostic_recovered();
        } else {
            self.connection_admission.clear_retained_quarantine();
        }
        quarantines.connection_entries.remove(&connection);
        true
    }

    fn connection_diagnostic_summary(
        &self,
    ) -> (connection::ConnectionStateCountSnapshot, Option<Duration>) {
        let quarantines = lock_unpoison(&self.quarantines);
        let counts = self.connection_admission.snapshot();
        let now = Instant::now();
        let oldest = quarantines
            .oldest
            .map(|started| now.saturating_duration_since(started));
        (counts, oldest)
    }

    fn connection_diagnostics(&self) -> Vec<RdmaConnectionDiagnostics> {
        let quarantined_tokens = lock_unpoison(&self.quarantines)
            .connection_entries
            .keys()
            .copied()
            .collect::<HashSet<_>>();
        let mut connections = self
            .connections
            .occupied()
            .into_iter()
            .map(|connection| {
                connection.diagnostics(quarantined_tokens.contains(&connection.token))
            })
            .collect::<Vec<_>>();
        connections.sort_by_key(|connection| {
            (
                connection.identity.registry_slot(),
                connection.identity.registration_generation(),
                connection.identity.qp_num(),
            )
        });
        connections
    }

    fn listener_diagnostics(&self) -> Vec<RdmaListenerDiagnostics> {
        self.cm.listener_diagnostics()
    }

    fn diagnostics(self: &Arc<Self>) -> RdmaEngineDiagnostics {
        let outcome = self.outcome();
        let (listener_count, queued_inbound_requests, pending_accepts, selected_accepts) =
            self.cm.listener_counts();
        let (connection_counts, oldest_quarantine_age) = self.connection_diagnostic_summary();
        RdmaEngineDiagnostics {
            lifecycle: self.lifecycle(),
            terminal_error: outcome.and_then(|outcome| outcome.summary()),
            device_name: self.config.device_name.clone(),
            completion_mode: self.config.completion_mode,
            maximum_live_connections: self.config.max_live_connections,
            maximum_inflight_operations: self.config.max_inflight_operations,
            cq_capacity: self.config.cq_capacity,
            cq_completion_budget: self.config.cq_completion_budget,
            cm_event_budget: self.config.cm_event_budget,
            reclamation_budget: self.config.reclamation_budget,
            ready_connection_quantum: self.config.ready_connection_quantum,
            shared_contexts: self.resources.contexts,
            shared_protection_domains: self.resources.protection_domains,
            shared_completion_queues: self.resources.completion_queues,
            shared_completion_channels: self.resources.completion_channels,
            shared_cq_notification_fds: self.resources.cq_notification_fds,
            shared_cm_event_channels: self.resources.cm_event_channels,
            shared_cm_event_fds: self.resources.cm_event_fds,
            explicit_engine_drivers: self.resources.explicit_drivers,
            library_owned_tasks: self.resources.library_owned_tasks,
            driver_wakeups: self.shared_driver_wakeups(),
            driver_yields: self.driver_yields.load(Ordering::Acquire),
            live_connection_reservations: connection_counts.live,
            establishing_connection_reservations: connection_counts.establishing,
            established_connection_reservations: connection_counts.established,
            draining_connection_reservations: connection_counts.draining,
            registered_live_qps: connection_counts.registered_live_qps,
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
            quarantined_bundles: connection_counts.quarantined_bundles,
            oldest_quarantine_age,
            ready_queue_depth: self.ready_queue_depth.load(Ordering::Acquire),
            listener_count,
            queued_inbound_requests,
            pending_accepts,
            selected_accepts,
            connections_admitted: self
                .diagnostic_counters
                .connections_admitted
                .load(Ordering::Acquire),
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
            operations_released_after_qp_destroy: self
                .diagnostic_counters
                .operations_released_after_qp_destroy
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
            connections_drain_started: self
                .diagnostic_counters
                .connections_drain_started
                .load(Ordering::Acquire),
            connections_drained: self
                .diagnostic_counters
                .connections_drained
                .load(Ordering::Acquire),
            connections_closed: self
                .diagnostic_counters
                .connections_closed
                .load(Ordering::Acquire),
            connections_quarantined: self
                .diagnostic_counters
                .connections_quarantined
                .load(Ordering::Acquire),
            connections_failed: self
                .diagnostic_counters
                .connections_failed
                .load(Ordering::Acquire),
            qp_error_transitions: self
                .diagnostic_counters
                .qp_error_transitions
                .load(Ordering::Acquire),
            qp_destroys: self.diagnostic_counters.qp_destroys.load(Ordering::Acquire),
            quarantine_recoveries: self
                .diagnostic_counters
                .quarantine_recoveries
                .load(Ordering::Acquire),
            connection_quarantine_outcomes: self
                .diagnostic_counters
                .connection_quarantine_outcomes
                .load(Ordering::Acquire),
            shutdowns: self.diagnostic_counters.shutdowns.load(Ordering::Acquire),
            engine_wedges: self
                .diagnostic_counters
                .engine_wedges
                .load(Ordering::Acquire),
            terminal_driver_errors: self
                .diagnostic_counters
                .terminal_driver_errors
                .load(Ordering::Acquire),
            cq_credits_reserved: self
                .diagnostic_counters
                .cq_credits_reserved
                .load(Ordering::Acquire),
            cq_credits_rolled_back: self
                .diagnostic_counters
                .cq_credits_rolled_back
                .load(Ordering::Acquire),
            cq_credits_released: self
                .diagnostic_counters
                .cq_credits_released
                .load(Ordering::Acquire),
            cq_credits_retained: self
                .diagnostic_counters
                .cq_credits_retained
                .load(Ordering::Acquire),
            listeners_created: self
                .diagnostic_counters
                .listeners_created
                .load(Ordering::Acquire),
            inbound_requests_accepted: self
                .diagnostic_counters
                .inbound_requests_accepted
                .load(Ordering::Acquire),
            inbound_requests_rejected: self
                .diagnostic_counters
                .inbound_requests_rejected
                .load(Ordering::Acquire),
            inbound_rejected_backlog_full: self
                .diagnostic_counters
                .inbound_rejected_backlog_full
                .load(Ordering::Acquire),
            inbound_rejected_connection_capacity: self
                .diagnostic_counters
                .inbound_rejected_connection_capacity
                .load(Ordering::Acquire),
            inbound_rejected_admission_closed: self
                .diagnostic_counters
                .inbound_rejected_admission_closed
                .load(Ordering::Acquire),
            inbound_rejected_listener_closed: self
                .diagnostic_counters
                .inbound_rejected_listener_closed
                .load(Ordering::Acquire),
            inbound_rejected_context_mismatch: self
                .diagnostic_counters
                .inbound_rejected_context_mismatch
                .load(Ordering::Acquire),
            inbound_rejected_setup_failure: self
                .diagnostic_counters
                .inbound_rejected_setup_failure
                .load(Ordering::Acquire),
            accept_cancellations_before_selection: self
                .diagnostic_counters
                .accept_cancellations_before_selection
                .load(Ordering::Acquire),
            accept_cancellations_after_selection: self
                .diagnostic_counters
                .accept_cancellations_after_selection
                .load(Ordering::Acquire),
            accept_setup_failures: self
                .diagnostic_counters
                .accept_setup_failures
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
            detail_source: diagnostics::DiagnosticsDetailSource(Arc::downgrade(self)),
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

    pub(crate) fn publish_connection_ready(&self, connection: &Arc<connection::ConnectionState>) {
        if connection.mark_ready_published() {
            lock_unpoison(&self.published_ready_connections).push_back(connection.token);
        }
        self.work_signal.publish(driver::READY_CONNECTION_WORK);
    }

    fn take_published_connection(&self) -> Option<registry::ConnectionToken> {
        let connection = lock_unpoison(&self.published_ready_connections).pop_front()?;
        if let registry::Lookup::Occupied(state) = self.connections.lookup(connection) {
            state.clear_ready_published();
        }
        Some(connection)
    }

    fn has_published_connections(&self) -> bool {
        !lock_unpoison(&self.published_ready_connections).is_empty()
    }

    fn update_ready_queue_depth(&self, scheduled: usize) {
        let published = lock_unpoison(&self.published_ready_connections).len();
        self.ready_queue_depth
            .store(scheduled.saturating_add(published), Ordering::Release);
    }

    fn schedule_reclamation(&self, token: OperationToken) {
        self.begin_reclamation(token);
        self.schedule_deadline(
            DeadlineKind::Reclamation,
            token.encode(),
            self.config.missing_cqe_deadline,
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
pub(crate) fn test_engine_pair(mode: CompletionMode) -> (RdmaEngine, RdmaEngineDriver) {
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
                cq_notification_fds: 0,
                cm_event_channels: 1,
                cm_event_fds: 1,
                explicit_drivers: 1,
                library_owned_tasks: 0,
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
