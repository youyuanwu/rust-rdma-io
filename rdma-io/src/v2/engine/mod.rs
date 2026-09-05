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
//! completion, provider-proven rejection, or successful synchronous
//! destruction of the owning QP establishes a positive safety boundary.
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

mod config;
mod diagnostics;
mod driver;
pub(crate) mod io;
mod io_core;
mod lifecycle;
mod registry;
mod resources;
mod scheduler;
mod session;

#[cfg(test)]
mod api_tests;

#[cfg(test)]
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use tokio::sync::Notify;

use config::EngineConfig;
pub use config::{CompletionMode, RdmaConnectionConfig};
pub use diagnostics::{RdmaEngineDiagnostics, RdmaEngineLifecycle, RdmaEngineTerminalError};
use driver::WorkSignal;
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub use driver::{
    TestAcceptedOperation, TestAdmissionBarrier, TestConnectionCqeSuppression, TestContextIdentity,
    TestCqArmWindowControl, TestCqeRejection, TestCqeSuppression, TestEngineInstrumentation,
    TestEngineQp, TestEngineResources, TestProviderLimits, TestRouteHandle,
    TestSharedResourceIdentity,
};
pub use io_core::RdmaOperation;
use io_core::{IoCore, IoDriverSignal};
use lifecycle::MemoizedTerminalResult;
use registry::{lock_unpoison, write_unpoison};
use resources::{EngineResourceRefs, EngineResources};
use scheduler::DeadlineKind;
use scheduler::WorkScheduler;
use session::SessionManager;
pub use session::connection::{RdmaConnection, RdmaConnectionIdentity};
pub use session::listener::{RdmaListener, RdmaListenerConfig};

use super::error::{Error, Result};

type ConnectionSetup =
    Box<dyn FnOnce(io::IoConnection, io::IoEventReceiver) -> Result<usize> + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SetupSummary {
    posted_wrs: usize,
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

    /// Set validated CQEs dispatched for one connection per turn in `1..=4096`.
    pub fn completion_dispatch_budget(mut self, value: usize) -> Self {
        self.config.completion_dispatch_budget = value;
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
        let resource_refs = resources.connection_resource_refs();
        let shared = EngineShared::new(self.config, Some(provider), Some(resource_refs))?;
        #[cfg(any(test, feature = "test-hooks"))]
        let shared = {
            let mut shared = shared;
            shared.test_resources = Some(resources.test_resource_refs());
            shared
        };
        let shared = shared.into_shared();
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
/// per-connection completion dispatch remains owned by the paired
/// [`RdmaEngineDriver`]. Message protocol progress belongs to each returned
/// [`crate::v2::MessageTransportDriver`].
/// The handle is `Clone + Send + Sync + 'static`.
///
/// Dropping the last `RdmaEngine` handle requests engine shutdown. Existing
/// [`RdmaConnection`], [`RdmaListener`], and message-transport handles retain
/// shared safety state but do not count as engine frontend handles and do not
/// prevent that shutdown request. Keep at least one engine clone alive until
/// new submissions are finished, and prefer [`RdmaEngine::shutdown`] when the
/// terminal result must be observed.
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

    /// Return an O(1), non-blocking lifecycle and safety-debt snapshot.
    ///
    /// The compact snapshot remains readable after terminal state and never
    /// scans individual connections or listeners.
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
        session::cm::connect(
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
        session::cm::connect(Arc::clone(&self.shared), address, config).await
    }

    pub(crate) async fn connect_with_io_setup<F>(
        &self,
        address: std::net::SocketAddr,
        config: RdmaConnectionConfig,
        setup: F,
    ) -> Result<RdmaConnection>
    where
        F: FnOnce(io::IoConnection, io::IoEventReceiver) -> Result<usize> + Send + 'static,
    {
        session::cm::connect_with_setup(Arc::clone(&self.shared), address, config, Box::new(setup))
            .await
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
        session::listener::listen(Arc::clone(&self.shared), address, config).await
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
/// CQ, reclamation/deadline, and per-connection completion dispatch. Message
/// protocol work belongs to [`crate::v2::MessageTransportDriver`]. Readiness
/// mode sleeps
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
    deadline_io_turn: bool,
    runtime_checked: bool,
}

struct EngineShared {
    config: EngineConfig,
    provider: Option<config::ProviderLimits>,
    // This engine-owned core retain drops before the root resources below.
    // Operation futures may extend the Arc, but each MR anchors its PD and an
    // engine with accepted work is retained fail-closed.
    io_core: Arc<IoCore>,
    session: Arc<SessionManager>,
    lifecycle: AtomicU8,
    shutdown_requested: AtomicBool,
    shutdown_deadline_scheduled: AtomicBool,
    failure_retained: AtomicBool,
    frontend_count: AtomicUsize,
    work_signal: Arc<WorkSignal>,
    // Notify stores only live Notified futures and wakes every concurrent
    // shutdown waiter without retaining registrations from dropped futures.
    terminal_notify: Notify,
    terminal: Mutex<Option<MemoizedTerminalResult>>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_resources: Option<resources::TestResourceRefs>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: Arc<driver::test_api::TestDriverState>,
    // Rust drops fields in declaration order. Keep this root retain after every
    // registry/test owner so quarantined QP/CM/MR descendants are released
    // before the shared CQ, PD, CM event channel, and context can disappear.
    resource_refs: Option<EngineResourceRefs>,
}

struct EngineIoDriverSignal {
    work_signal: Arc<WorkSignal>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: Arc<driver::test_api::TestDriverState>,
}

impl IoDriverSignal for EngineIoDriverSignal {
    fn publish_cq_recheck(&self) {
        self.work_signal.publish(driver::CQ_RECHECK_WORK);
    }

    fn publish_completion_dispatch(&self) {
        self.work_signal.publish(driver::COMPLETION_DISPATCH_WORK);
    }

    fn publish_reclamation(&self) {
        self.work_signal.publish(driver::RECLAMATION_WORK);
    }

    fn publish_terminal(&self) {
        self.work_signal.publish(driver::TERMINAL_WORK);
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn pause_operation_before_register(&self) {
        self.test_driver
            .pause_admission(driver::test_api::AdmissionPausePoint::OperationBeforeRegister);
    }
}

#[cfg(test)]
impl Deref for EngineShared {
    // Session state is physically owned by SessionManager. Existing internal
    // modules are migrated receiver-by-receiver without re-exposing those
    // fields on the composition root.
    type Target = SessionManager;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl EngineShared {
    fn into_shared(self) -> Arc<Self> {
        let shared = Arc::new(self);
        shared.session.bind_engine(&shared);
        shared
    }

    fn new(
        config: EngineConfig,
        provider: Option<config::ProviderLimits>,
        resource_refs: Option<EngineResourceRefs>,
    ) -> Result<Self> {
        let admission = Arc::new(RwLock::new(()));
        let work_signal = Arc::new(WorkSignal::new());
        #[cfg(any(test, feature = "test-hooks"))]
        let test_driver = Arc::new(driver::test_api::TestDriverState::new());
        let io_driver_signal: Arc<dyn IoDriverSignal> = Arc::new(EngineIoDriverSignal {
            work_signal: Arc::clone(&work_signal),
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver: Arc::clone(&test_driver),
        });
        let (io_core, qp_reclaim) = IoCore::new(
            config.max_inflight_operations,
            config.cq_capacity,
            config.missing_cqe_deadline,
            config.completion_dispatch_budget,
            Arc::clone(&admission),
            io_driver_signal,
        )?;
        let session = Arc::new(SessionManager::new(
            config.max_live_connections,
            Arc::clone(&admission),
            Arc::clone(&io_core),
            qp_reclaim,
        )?);
        Ok(Self {
            config,
            provider,
            io_core,
            session,
            lifecycle: AtomicU8::new(lifecycle_to_u8(RdmaEngineLifecycle::Created)),
            shutdown_requested: AtomicBool::new(false),
            shutdown_deadline_scheduled: AtomicBool::new(false),
            failure_retained: AtomicBool::new(false),
            frontend_count: AtomicUsize::new(1),
            work_signal,
            terminal_notify: Notify::new(),
            terminal: Mutex::new(None),
            #[cfg(any(test, feature = "test-hooks"))]
            test_resources: None,
            #[cfg(any(test, feature = "test-hooks"))]
            test_driver,
            resource_refs,
        })
    }

    fn request_shutdown(&self) {
        #[cfg(any(test, feature = "test-hooks"))]
        self.test_driver.record_shutdown_attempt();
        self.mark_shutdown_requested();
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
        let _admission = write_unpoison(&self.session.admission);
        if self.shutdown_requested.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.io_core.close_admission(Some(Error::DriverShutdown));
        self.transition_shutdown_requested();
        true
    }

    fn finish(&self, outcome: MemoizedTerminalResult) {
        assert!(
            !outcome.is_connection_quarantined(),
            "ConnectionQuarantined is connection-local; no connection quarantine can terminate the engine driver"
        );
        let (mut io_effects, connections_to_wake) = {
            let _admission = write_unpoison(&self.session.admission);
            let mut terminal = lock_unpoison(&self.terminal);
            if terminal.is_some() {
                return;
            }
            self.shutdown_requested.store(true, Ordering::Release);
            self.io_core.close_admission(outcome.error());
            self.transition_shutdown_requested();
            let lifecycle = if outcome.is_success() {
                RdmaEngineLifecycle::Terminated
            } else {
                RdmaEngineLifecycle::Failed
            };
            *terminal = Some(outcome.clone());
            self.transition_terminal(lifecycle);

            let io_effects = self.io_core.terminalize_operations(&outcome);

            let connections_to_wake = self.session.connections.occupied();
            drop(terminal);
            (io_effects, connections_to_wake)
        };

        self.session.apply_io_effects(self, &mut io_effects);
        self.session.terminalize_cm(&outcome);
        for connection in &connections_to_wake {
            if outcome.is_error() && connection.retain_bundle_for_engine_failure() {
                self.session.track_connection_quarantine(connection.token);
            }
            if let Some(event) = self
                .session
                .finalize_connection_engine(connection, &outcome)
            {
                event.deliver();
            }
        }
        io_effects.publish();
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
        self.session
            .live_connection_count()
            .max(self.session.cm.retained_owner_count())
    }

    fn unsafe_outstanding_operations(&self) -> usize {
        self.io_core.accepted_count()
    }

    fn retain_after_failure(shared: &Arc<Self>) {
        if (shared.unsafe_outstanding_operations() == 0
            && shared.session.live_connection_count() == 0
            && shared.session.cm.retained_owner_count() == 0)
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
        let connection_counts = self.session.connection_admission.snapshot();
        let io = self.io_core.diagnostics();
        RdmaEngineDiagnostics {
            lifecycle: self.lifecycle(),
            terminal_error: self.outcome().and_then(|outcome| outcome.summary()),
            live_connections: connection_counts.live,
            registered_operations: io.registered_operations,
            accepted_operations: io.accepted_operations,
            pending_reclamations: io.pending_reclamations,
            available_cq_credits: io.available_cq_credits,
            retained_cq_credits: io.retained_cq_credits,
            quarantined_operations: io.quarantined_operations,
            quarantined_mrs: io.quarantined_mrs,
            quarantined_bytes: io.quarantined_bytes,
            quarantined_connections: connection_counts.quarantined_bundles,
        }
    }

    #[cfg(test)]
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

    #[cfg(test)]
    fn has_published_completions(&self) -> bool {
        self.io_core.has_published_connections()
    }

    fn schedule_deadline(&self, kind: DeadlineKind, token: u64, after: Duration) {
        self.session
            .schedule_deadline(&self.work_signal, kind, token, after);
    }

    #[cfg(test)]
    fn apply_io_effects(&self, effects: &mut io_core::IoCoreEffects) {
        self.session.apply_io_effects(self, effects);
    }

    #[cfg(test)]
    pub(super) fn enqueue_completion(
        &self,
        completion: crate::wc::WorkCompletion,
    ) -> Option<registry::ConnectionToken> {
        self.session.enqueue_completion(completion)
    }

    #[cfg(test)]
    pub(super) fn dispatch_connection_completions(
        &self,
        token: registry::ConnectionToken,
        quantum: usize,
    ) -> (usize, bool) {
        self.session
            .dispatch_connection_completions(self, token, quantum)
    }

    #[cfg(test)]
    pub(super) fn reclaim_after_qp_destroy(
        &self,
        proof: &session::QpDestructionProof,
        connection: &session::connection::ConnectionState,
        token: registry::OperationToken,
    ) -> bool {
        self.session
            .reclaim_after_qp_destroy_for_test(self, proof, connection, token)
    }

    #[cfg(test)]
    pub(super) fn handle_reclamation_deadline(&self, token: registry::OperationToken) {
        self.session.handle_reclamation_deadline(self, token);
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
    let shared = EngineShared::new(config, None, None).unwrap().into_shared();
    (
        RdmaEngine {
            shared: Arc::clone(&shared),
        },
        RdmaEngineDriver::new(shared, None),
    )
}
