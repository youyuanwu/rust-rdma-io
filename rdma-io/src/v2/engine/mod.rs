//! Explicitly driven, shared v2 RDMA engine.
//!
//! One engine owns one anchored verbs context facade, one protection domain,
//! one send/receive CQ, one CM event channel, and mode-specific CQ
//! notification resources. Readiness owns one completion channel/fd; polling
//! owns none. Every connection shares those objects.
//!
//! The driver exclusively owns one bounded reactor and follows each turn with
//! one terminal-eligibility epilogue. The I/O state polls the
//! shared CQ and validates a CQE only when the
//! current connection generation, operation generation, operation owner, and
//! provider-reported `qp_num` all agree. The session owner consumes CM events
//! and controls connection lifecycle. Cancellation, close, shutdown, and
//! driver loss retain accepted or acceptance-ambiguous MRs until an exact
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
mod progress;
mod reactor;
mod registry;
mod resources;
mod scheduler;
mod session;

#[cfg(test)]
mod api_tests;

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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
use io_core::{IoCoreDiagnostics, IoDriverSignal};
use lifecycle::MemoizedTerminalResult;
use reactor::{CommandIngress, EngineReactor};
use registry::{lock_unpoison, write_unpoison};
use resources::EngineReactorResources;
pub use session::connection::{RdmaConnection, RdmaConnectionIdentity};
pub use session::listener::{RdmaListener, RdmaListenerConfig};
use session::{SessionFrontend, SessionManager};

use super::error::{Error, Result};

type ConnectionSetup =
    Box<dyn for<'a> FnOnce(io::BorrowedSetupIo<'a>, io::IoEventReceiver) -> Result<usize> + Send>;

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

    /// Set I/O reclamation/deadline actions per I/O service turn in `1..=4096`.
    ///
    /// This and [`Self::session_reclamation_budget`] replace the former
    /// aggregate v2 `reclamation_budget`. To preserve an old aggregate value
    /// `N`, divide it between the two owner-local controls. Odd values may use
    /// either floor/ceiling assignment. The old value `1` has no exact
    /// equivalent because both owners require a nonzero bounded turn; the
    /// minimum replacement is `(1, 1)`.
    pub fn io_reclamation_budget(mut self, value: usize) -> Self {
        self.config.io_reclamation_budget = value;
        self
    }

    /// Set session reclamation/deadline actions per session turn in `1..=4096`.
    ///
    /// See [`Self::io_reclamation_budget`] for migration from the removed
    /// aggregate `reclamation_budget` control.
    pub fn session_reclamation_budget(mut self, value: usize) -> Self {
        self.config.session_reclamation_budget = value;
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

        let (resources, provider) = EngineReactorResources::build(&self.config)?;
        let memory = resources.memory_registrar();
        let (shared, session) = EngineFrontendRoot::new(self.config, Some(provider), memory)?;
        #[cfg(any(test, feature = "test-hooks"))]
        let shared = {
            let mut shared = shared;
            shared.test_observers = Some(resources.test_resource_observers());
            shared
        };
        let shared = shared.into_shared();
        let engine = RdmaEngine {
            shared: Arc::clone(&shared),
        };
        let driver = RdmaEngineDriver::new(shared, session, Some(resources));
        Ok((engine, driver))
    }
}

/// Cloneable frontend for one explicitly driven engine instance.
///
/// Cloning this value never starts work. The paired [`RdmaEngineDriver`]
/// schedules bounded turns while the I/O core owns CQ/completion/reclamation
/// policy and the session subsystem owns CM and connection lifecycle policy.
/// Message protocol progress belongs to each returned
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
    shared: Arc<EngineFrontendRoot>,
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
        self.shared.request_shutdown_command();
        loop {
            let notified = self.shared.observer.terminal_notify.notified();
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
    /// configuration. The engine driver schedules every bounded CM and CQ
    /// progress turn through its owning layer.
    ///
    /// Low-level establishment posts zero initial receives. With the default
    /// infinite RNR retry, a peer's early send can wait until the application
    /// posts a receive.
    pub async fn connect(&self, address: std::net::SocketAddr) -> Result<RdmaConnection> {
        session::cm::connect(
            Arc::clone(&self.shared.session),
            Arc::clone(&self.shared.commands),
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
        session::cm::connect(
            Arc::clone(&self.shared.session),
            Arc::clone(&self.shared.commands),
            address,
            config,
        )
        .await
    }

    pub(crate) async fn connect_with_io_setup<F>(
        &self,
        address: std::net::SocketAddr,
        config: RdmaConnectionConfig,
        setup: F,
    ) -> Result<RdmaConnection>
    where
        F: for<'a> FnOnce(io::BorrowedSetupIo<'a>, io::IoEventReceiver) -> Result<usize>
            + Send
            + 'static,
    {
        session::cm::connect_with_setup(
            Arc::clone(&self.shared.session),
            Arc::clone(&self.shared.commands),
            address,
            config,
            Box::new(setup),
        )
        .await
    }

    pub(crate) fn validate_message_connection_config(
        &self,
        config: &RdmaConnectionConfig,
    ) -> Result<()> {
        self.shared.session.validate_connection_config(config)
    }

    /// Bind an engine-owned listener on the shared CM event channel.
    ///
    /// `config.backlog_capacity()` must be in `1..=4096`. The validated value
    /// is passed unchanged to `rdma_listen` and also bounds admitted accept
    /// requests and pending inbound children. Provider refusal is returned as
    /// a contextual listener-creation error.
    pub async fn listen(
        &self,
        address: std::net::SocketAddr,
        config: RdmaListenerConfig,
    ) -> Result<RdmaListener> {
        session::listener::listen(
            Arc::clone(&self.shared.session),
            Arc::clone(&self.shared.commands),
            address,
            config,
        )
        .await
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_resources(&self) -> Result<driver::TestEngineResources> {
        let resources =
            self.shared.test_observers.clone().ok_or_else(|| {
                Error::InvalidConfig("test engine resources are unavailable".into())
            })?;
        Ok(driver::TestEngineResources::new(&self.shared, resources))
    }
}

impl Drop for RdmaEngine {
    fn drop(&mut self) {
        if self.shared.frontend_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.request_shutdown_command();
        }
    }
}

/// Sole progress future for an [`RdmaEngine`].
///
/// The driver fairly rotates across the bounded sources of one owned reactor.
/// Every external poll probes all sources, services each ready-at-entry source
/// at most once, and then composes terminal eligibility. CQ polling, completion
/// dispatch, and operation deadlines remain behind the I/O owner; CM progress,
/// lifecycle deadlines, and teardown remain behind the session owner. Message
/// protocol work belongs to [`crate::v2::MessageTransportDriver`]. Readiness
/// mode sleeps only after registering and rechecking event sources and
/// published software work; polling mode performs one bounded nonblocking
/// iteration followed by a cooperative yield.
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
    shared: Arc<EngineFrontendRoot>,
    reactor: EngineReactor,
    deadline_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    deadline_at: Option<tokio::time::Instant>,
    runtime_checked: bool,
}

/// Resource-free lifecycle/result observation published by `EngineReactor`.
struct EngineObserver {
    lifecycle: AtomicU8,
    terminal_notify: Arc<Notify>,
    terminal: Mutex<Option<MemoizedTerminalResult>>,
}

impl EngineObserver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lifecycle: AtomicU8::new(lifecycle_to_u8(RdmaEngineLifecycle::Created)),
            terminal_notify: Arc::new(Notify::new()),
            terminal: Mutex::new(None),
        })
    }

    fn publish_lifecycle(&self, lifecycle: RdmaEngineLifecycle) {
        self.lifecycle
            .store(lifecycle_to_u8(lifecycle), Ordering::Release);
    }

    fn publish_terminal_into(
        &self,
        outcome: MemoizedTerminalResult,
        actions: &mut reactor::ReactorActions,
    ) {
        let mut terminal = lock_unpoison(&self.terminal);
        if terminal.is_some() {
            return;
        }
        *terminal = Some(outcome);
        drop(terminal);
        let terminal_notify = Arc::clone(&self.terminal_notify);
        actions.push_terminal(move || terminal_notify.notify_waiters());
    }

    fn outcome(&self) -> Option<MemoizedTerminalResult> {
        lock_unpoison(&self.terminal).clone()
    }

    fn lifecycle(&self) -> RdmaEngineLifecycle {
        lifecycle_from_u8(self.lifecycle.load(Ordering::Acquire))
    }
}

/// Resource-free failure/admission ingress; it owns no lifecycle or terminal result.
struct EngineControl {
    driver_failure: Mutex<Option<Error>>,
    admission: Arc<RwLock<()>>,
    commands: std::sync::Weak<CommandIngress>,
    observer: std::sync::Weak<EngineObserver>,
    work_signal: std::sync::Weak<WorkSignal>,
}

impl EngineControl {
    fn new(
        admission: Arc<RwLock<()>>,
        commands: &Arc<CommandIngress>,
        observer: &Arc<EngineObserver>,
        work_signal: &Arc<WorkSignal>,
    ) -> Arc<Self> {
        Arc::new(Self {
            driver_failure: Mutex::new(None),
            admission,
            commands: Arc::downgrade(commands),
            observer: Arc::downgrade(observer),
            work_signal: Arc::downgrade(work_signal),
        })
    }

    fn request_driver_failure(&self, error: Error) {
        let mut pending = lock_unpoison(&self.driver_failure);
        let terminal = self
            .observer
            .upgrade()
            .and_then(|observer| observer.outcome());
        if pending.is_none() && terminal.is_none() {
            *pending = Some(error.clone());
            let admission = write_unpoison(&self.admission);
            if let Some(commands) = self.commands.upgrade() {
                commands.close_admission_with(error);
            }
            drop(admission);
            if let Some(work_signal) = self.work_signal.upgrade() {
                work_signal.publish(driver::IO_WORK | driver::SESSION_WORK);
            }
        }
    }

    fn take_driver_failure(&self) -> Option<Error> {
        lock_unpoison(&self.driver_failure).take()
    }

    #[cfg(test)]
    fn pending_terminal_outcome(&self) -> Option<MemoizedTerminalResult> {
        lock_unpoison(&self.driver_failure)
            .clone()
            .map(MemoizedTerminalResult::from_error)
    }

    fn publish(&self, work: usize) {
        if let Some(work_signal) = self.work_signal.upgrade() {
            work_signal.publish(work);
        }
    }
}

/// Resource-free composition root shared by public frontends and the driver.
///
/// Provider resources and mutable runtime policy are owned only by
/// `EngineReactor`. This root contains ingress, terminal observation,
/// immutable diagnostics snapshots, and explicitly shareable frontend
/// capabilities.
struct EngineFrontendRoot {
    config: EngineConfig,
    // Runtime-state-free policy retained by the frontend composition root.
    // Provider ownership remains exclusively in EngineReactor.
    session: Arc<SessionFrontend>,
    io_diagnostics: Mutex<IoCoreDiagnostics>,
    connection_diagnostics: Mutex<session::connection::ConnectionStateCountSnapshot>,
    cm_diagnostics: Mutex<(usize, usize)>,
    #[cfg(any(test, feature = "test-hooks"))]
    cm_rejections: std::sync::atomic::AtomicU64,
    #[cfg(any(test, feature = "test-hooks"))]
    io_rejections: Mutex<Vec<io_core::CqeReject>>,
    // Resource-free frontend observation published by the driver-owned
    // lifecycle state.
    observer: Arc<EngineObserver>,
    control: Arc<EngineControl>,
    frontend_count: AtomicUsize,
    work_signal: Arc<WorkSignal>,
    commands: Arc<CommandIngress>,
    #[cfg(any(test, feature = "test-hooks"))]
    // Weak provider identities used only for deterministic test observation.
    test_observers: Option<resources::TestResourceObservers>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: Arc<driver::test_api::TestDriverState>,
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone)]
struct SessionTestInstrumentation {
    driver: Arc<driver::test_api::TestDriverState>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl SessionTestInstrumentation {
    fn pause_connect_before_enqueue(&self) {
        self.driver
            .pause_admission(driver::test_api::AdmissionPausePoint::ConnectBeforeEnqueue);
    }

    fn take_setup_rollback_failure(&self) -> Option<driver::test_api::SetupRollbackFailure> {
        self.driver.take_setup_rollback_failure()
    }
}

struct EngineIoDriverSignal {
    work_signal: Arc<WorkSignal>,
    #[cfg(any(test, feature = "test-hooks"))]
    test_driver: Arc<driver::test_api::TestDriverState>,
}

impl IoDriverSignal for EngineIoDriverSignal {
    fn publish_cq_recheck(&self) {
        self.work_signal.publish(driver::IO_WORK);
    }

    fn publish_completion_dispatch(&self) {
        self.work_signal.publish(driver::IO_WORK);
    }

    fn publish_reclamation(&self) {
        self.work_signal.publish(driver::IO_WORK);
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn pause_operation_before_register(&self) {
        self.test_driver
            .pause_admission(driver::test_api::AdmissionPausePoint::OperationBeforeRegister);
    }
}

impl EngineFrontendRoot {
    fn into_shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn new(
        config: EngineConfig,
        provider: Option<config::ProviderLimits>,
        memory: io::MemoryRegistrar,
    ) -> Result<(Self, SessionManager)> {
        let admission = Arc::new(RwLock::new(()));
        let work_signal = Arc::new(WorkSignal::new());
        let commands = CommandIngress::new(
            config.max_live_connections,
            config.max_inflight_operations,
            Arc::clone(&work_signal),
        );
        let observer = EngineObserver::new();
        let control =
            EngineControl::new(Arc::clone(&admission), &commands, &observer, &work_signal);
        #[cfg(any(test, feature = "test-hooks"))]
        let test_driver = Arc::new(driver::test_api::TestDriverState::new());
        let session = SessionManager::new(
            config::SessionConfig::from(&config),
            provider,
            Arc::clone(&admission),
            memory,
            Arc::downgrade(&control),
            #[cfg(any(test, feature = "test-hooks"))]
            SessionTestInstrumentation {
                driver: Arc::clone(&test_driver),
            },
        )?;
        session.bind_self();
        session.bind_commands(&commands);
        session.bind_engine(&observer, &work_signal);
        let session_frontend = session.frontend();
        let initial_cq_credits = config.cq_capacity;
        Ok((
            Self {
                config,
                session: session_frontend,
                io_diagnostics: Mutex::new(IoCoreDiagnostics {
                    registered_operations: 0,
                    accepted_operations: 0,
                    pending_reclamations: 0,
                    available_cq_credits: initial_cq_credits,
                    retained_cq_credits: 0,
                    quarantined_operations: 0,
                    quarantined_mrs: 0,
                    quarantined_bytes: 0,
                }),
                connection_diagnostics: Mutex::new(
                    session::connection::ConnectionStateCountSnapshot::default(),
                ),
                cm_diagnostics: Mutex::new((0, 0)),
                #[cfg(any(test, feature = "test-hooks"))]
                cm_rejections: std::sync::atomic::AtomicU64::new(0),
                #[cfg(any(test, feature = "test-hooks"))]
                io_rejections: Mutex::new(Vec::new()),
                observer,
                control,
                frontend_count: AtomicUsize::new(1),
                work_signal,
                commands,
                #[cfg(any(test, feature = "test-hooks"))]
                test_observers: None,
                #[cfg(any(test, feature = "test-hooks"))]
                test_driver,
            },
            session,
        ))
    }

    fn request_shutdown_command(&self) {
        // Admission closes synchronously so no old-path operation can cross
        // the shutdown boundary before the driver consumes the control
        // command. Provider and teardown progress still require the driver.
        #[cfg(any(test, feature = "test-hooks"))]
        self.test_driver.record_shutdown_attempt();
        let admission = write_unpoison(&self.session.admission);
        self.commands.close_admission_with(Error::DriverShutdown);
        drop(admission);
        self.commands.request_shutdown();
    }

    #[cfg(test)]
    fn request_shutdown(&self) {
        #[cfg(any(test, feature = "test-hooks"))]
        self.test_driver.record_shutdown_attempt();
        let admission = write_unpoison(&self.session.admission);
        self.commands.close_admission_with(Error::DriverShutdown);
        drop(admission);
        self.commands.request_shutdown();
    }

    fn start_shutdown_progress(&self) {
        self.work_signal
            .publish(driver::IO_WORK | driver::SESSION_WORK);
    }

    fn publish_lifecycle(&self, lifecycle: RdmaEngineLifecycle) {
        self.observer.publish_lifecycle(lifecycle);
    }

    fn publish_terminal_into(
        &self,
        outcome: MemoizedTerminalResult,
        actions: &mut reactor::ReactorActions,
    ) {
        self.observer.publish_terminal_into(outcome, actions);
    }

    #[cfg(test)]
    fn request_driver_failure(&self, error: Error) {
        self.control.request_driver_failure(error);
    }

    fn take_driver_failure(&self) -> Option<Error> {
        self.control.take_driver_failure()
    }

    #[cfg(test)]
    fn begin_driver_failure(&self, error: Error) {
        self.request_driver_failure(error);
    }

    fn outcome(&self) -> Option<MemoizedTerminalResult> {
        self.observer.outcome()
    }

    fn lifecycle(&self) -> RdmaEngineLifecycle {
        self.observer.lifecycle()
    }

    fn admission_error(&self) -> Option<Error> {
        if let Some(outcome) = self.outcome() {
            return outcome.into_result().err();
        }
        self.commands.admission_error()
    }

    fn diagnostics(&self) -> RdmaEngineDiagnostics {
        let connection_counts = *lock_unpoison(&self.connection_diagnostics);
        let io = *lock_unpoison(&self.io_diagnostics);
        RdmaEngineDiagnostics {
            lifecycle: self.lifecycle(),
            terminal_error: self.outcome().and_then(|outcome| outcome.summary()),
            live_connections: connection_counts
                .live
                .max(self.commands.connection_reservations()),
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

    fn update_io_diagnostics(&self, diagnostics: IoCoreDiagnostics) {
        *lock_unpoison(&self.io_diagnostics) = diagnostics;
    }

    fn update_connection_diagnostics(
        &self,
        diagnostics: session::connection::ConnectionStateCountSnapshot,
    ) {
        *lock_unpoison(&self.connection_diagnostics) = diagnostics;
    }

    fn update_cm_diagnostics(&self, pending_routes: usize, retained_owners: usize) {
        *lock_unpoison(&self.cm_diagnostics) = (pending_routes, retained_owners);
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn update_cm_rejections(&self, rejected: u64) {
        self.cm_rejections.store(rejected, Ordering::Release);
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn update_io_rejections(&self, rejections: Vec<io_core::CqeReject>) {
        *lock_unpoison(&self.io_rejections) = rejections;
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
pub(crate) fn test_engine_pair(mode: CompletionMode) -> (RdmaEngine, RdmaEngineDriver) {
    test_engine_pair_with_capacity(mode, EngineConfig::new("test0".into()).max_live_connections)
}

#[cfg(test)]
pub(crate) fn test_engine_pair_with_capacity(
    mode: CompletionMode,
    max_live_connections: usize,
) -> (RdmaEngine, RdmaEngineDriver) {
    let mut config = EngineConfig::new("test0".into());
    config.completion_mode = mode;
    config.max_live_connections = max_live_connections;
    let (shared, session) =
        EngineFrontendRoot::new(config, None, io::MemoryRegistrar::from_pd(None)).unwrap();
    let shared = shared.into_shared();
    (
        RdmaEngine {
            shared: Arc::clone(&shared),
        },
        RdmaEngineDriver::new(shared, session, None),
    )
}
