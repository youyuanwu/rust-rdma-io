//! Sole engine-progress future and wakeup protocol.
//!
//! Software producers publish queue state before setting a pending class bit
//! and incrementing an epoch, then wake the registered `AtomicWaker`. Before
//! returning `Pending`, the driver registers its waker and rechecks both the
//! pending bits and epoch. Therefore a publish before registration is found by
//! the recheck, while a publish after registration performs the wake.
//!
//! Readiness CQ progress polls before arming, arms once, immediately polls
//! again, waits for the shared fd only after both polls are empty, then drains
//! and acknowledges every channel event before repeating. This closes both CQ
//! edge races without a periodic timer. Polling mode performs one bounded CQ
//! attempt per driver poll and returns `Pending` after scheduling a cooperative
//! wake; neither mode scans idle connection registrations.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use crate::wc::WorkCompletion;

use super::config::CompletionMode;
use super::resources::EngineResources;
use super::scheduler::{WorkClass, WorkScheduler};
use super::{EngineFailure, EngineOutcome, EngineShared, RdmaEngineDriver};
use crate::v2::completion::CqReadiness;
use crate::v2::error::{Error, Result};

pub(super) const TERMINAL_WORK: usize = 1 << 0;
const RECLAMATION_WORK: usize = 1 << 1;
const READY_CONNECTION_WORK: usize = 1 << 2;
const WORK_CLASS_COUNT: usize = 5;

pub(super) struct WorkSignal {
    pending: std::sync::atomic::AtomicUsize,
    epoch: std::sync::atomic::AtomicU64,
    waker: futures_util::task::AtomicWaker,
}

impl WorkSignal {
    pub(super) fn new() -> Self {
        Self {
            pending: std::sync::atomic::AtomicUsize::new(0),
            epoch: std::sync::atomic::AtomicU64::new(0),
            waker: futures_util::task::AtomicWaker::new(),
        }
    }

    pub(super) fn publish(&self, work: usize) {
        self.pending
            .fetch_or(work, std::sync::atomic::Ordering::Release);
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.waker.wake();
    }

    fn take(&self) -> usize {
        self.pending.swap(0, std::sync::atomic::Ordering::AcqRel)
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    fn register_and_recheck(&self, waker: &std::task::Waker, observed_epoch: u64) -> usize {
        self.waker.register(waker);
        let pending = self.take();
        if pending != 0 || self.epoch() != observed_epoch {
            waker.wake_by_ref();
        }
        pending
    }
}

impl RdmaEngineDriver {
    pub(super) fn new(shared: Arc<EngineShared>, resources: Option<EngineResources>) -> Self {
        let cq_budget = shared.config.cq_completion_budget;
        Self {
            shared,
            resources,
            scheduler: WorkScheduler::new(),
            cq_readiness: CqReadiness::default(),
            cq_buffer: vec![WorkCompletion::default(); cq_budget].into_boxed_slice(),
            time_context_checked: false,
        }
    }

    fn mark_published_work(&mut self, published: usize) {
        if published & TERMINAL_WORK != 0 {
            self.scheduler.mark_class_ready(WorkClass::Terminal);
        }
        if published & RECLAMATION_WORK != 0 {
            self.scheduler.mark_class_ready(WorkClass::Reclamation);
        }
        if published & READY_CONNECTION_WORK != 0 {
            self.scheduler.mark_class_ready(WorkClass::ReadyConnection);
        }
    }

    fn fail(&mut self, failure: EngineFailure) -> Poll<Result<()>> {
        let outcome = EngineOutcome::Failure(failure);
        self.shared.finish(outcome.clone());
        self.release_resources();
        Poll::Ready(outcome.into_result())
    }

    fn release_resources(&mut self) {
        if let Some(resources) = self.resources.as_mut() {
            resources.drop_readiness_adapters();
        }
        self.resources.take();
    }

    fn service_terminal(&mut self) -> bool {
        if !self
            .shared
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }

        #[cfg(any(test, feature = "test-hooks"))]
        if self.shared.test_driver.accepted_outstanding() != 0 {
            return false;
        }

        self.shared.finish(EngineOutcome::Success);
        true
    }

    fn service_cq(&mut self, cx: &mut TaskContext<'_>) -> Result<bool> {
        let Some(resources) = self.resources.as_ref() else {
            return Ok(false);
        };
        let count = match self.shared.config.completion_mode {
            CompletionMode::Readiness => {
                let async_fd = resources.cq_async_fd.as_ref().ok_or_else(|| {
                    Error::InvalidConfig("readiness engine has no CQ AsyncFd".into())
                })?;
                #[cfg(any(test, feature = "test-hooks"))]
                let arms_before = self.cq_readiness.arm_count();
                let polled = self.cq_readiness.poll_with_async_fd(
                    &resources.cq,
                    async_fd,
                    cx,
                    &mut self.cq_buffer,
                );
                #[cfg(any(test, feature = "test-hooks"))]
                if self.cq_readiness.arm_count() != arms_before {
                    self.shared
                        .test_driver
                        .record_cq_arms(self.cq_readiness.arm_count() - arms_before);
                }
                match polled {
                    Poll::Ready(result) => result?,
                    Poll::Pending => return Ok(false),
                }
            }
            CompletionMode::Polling => resources.cq.poll(&mut self.cq_buffer)?,
        };

        if count == 0 {
            return Ok(false);
        }
        for completion in self.cq_buffer[..count].iter().copied() {
            #[cfg(any(test, feature = "test-hooks"))]
            self.shared.test_driver.dispatch(completion);
            #[cfg(not(any(test, feature = "test-hooks")))]
            let _ = completion;
        }
        self.scheduler.mark_class_ready(WorkClass::Cq);
        Ok(true)
    }

    fn service_cm(&mut self, cx: &mut TaskContext<'_>) -> Result<bool> {
        let Some(resources) = self.resources.as_ref() else {
            return Ok(false);
        };

        let mut readiness_guard = match self.shared.config.completion_mode {
            CompletionMode::Readiness => {
                let async_fd = resources.cm_async_fd.as_ref().ok_or_else(|| {
                    Error::InvalidConfig("readiness engine has no CM AsyncFd".into())
                })?;
                match async_fd.poll_read_ready(cx) {
                    Poll::Ready(Ok(guard)) => Some(guard),
                    Poll::Ready(Err(error)) => return Err(Error::Verbs(error)),
                    Poll::Pending => return Ok(false),
                }
            }
            CompletionMode::Polling => None,
        };

        let mut processed = 0usize;
        let mut reached_empty = false;
        while processed < self.shared.config.cm_event_budget {
            match resources.cm_event_channel.try_get_event() {
                Ok(event) => {
                    event.ack();
                    processed += 1;
                }
                Err(crate::Error::WouldBlock) => {
                    reached_empty = true;
                    break;
                }
                Err(crate::Error::Verbs(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    reached_empty = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }

        if reached_empty {
            if let Some(guard) = readiness_guard.as_mut() {
                guard.clear_ready();
            }
        } else if processed == self.shared.config.cm_event_budget {
            self.scheduler.mark_class_ready(WorkClass::Cm);
        }
        Ok(processed > 0)
    }

    fn service_reclamation(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        let due = self
            .scheduler
            .deadlines()
            .pop_due(now, self.shared.config.reclamation_budget);
        if self
            .scheduler
            .deadlines()
            .next()
            .is_some_and(|at| at <= now)
        {
            self.scheduler.mark_class_ready(WorkClass::Reclamation);
        }
        !due.is_empty()
    }

    fn service_ready_connection(&mut self) -> bool {
        let Some(connection) = self.scheduler.pop_connection() else {
            return false;
        };
        let _quantum = self.shared.config.ready_connection_quantum;
        let _ = connection;
        if self.scheduler.ready_connection_count() > 0 {
            self.scheduler.mark_class_ready(WorkClass::ReadyConnection);
        }
        true
    }
}

impl Future for RdmaEngineDriver {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if let Some(outcome) = self.shared.outcome() {
            self.release_resources();
            return Poll::Ready(outcome.into_result());
        }

        if !self.time_context_checked {
            if let Err(error) = super::preflight_tokio_time() {
                return self.fail(EngineFailure::InvalidRuntime(error));
            }
            self.time_context_checked = true;
            self.shared.transition_running();
        }

        let observed_epoch = self.shared.work_signal.epoch();
        let published = self.shared.work_signal.take();
        self.mark_published_work(published);
        if self
            .shared
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.scheduler.mark_class_ready(WorkClass::Terminal);
        }
        self.scheduler.mark_class_ready(WorkClass::Cm);
        self.scheduler.mark_class_ready(WorkClass::Cq);

        let class_budget = self.scheduler.ready_class_count().min(WORK_CLASS_COUNT);
        for _ in 0..class_budget {
            let Some(class) = self.scheduler.next_class() else {
                break;
            };
            let result = match class {
                WorkClass::Terminal => {
                    if self.service_terminal() {
                        break;
                    }
                    Ok(false)
                }
                WorkClass::Cm => self.service_cm(cx),
                WorkClass::Cq => self.service_cq(cx),
                WorkClass::Reclamation => Ok(self.service_reclamation()),
                WorkClass::ReadyConnection => Ok(self.service_ready_connection()),
            };
            if let Err(error) = result {
                return self.fail(EngineFailure::Progress(error.to_string()));
            }
            if self.shared.outcome().is_some() {
                break;
            }
        }

        if let Some(outcome) = self.shared.outcome() {
            self.release_resources();
            return Poll::Ready(outcome.into_result());
        }

        match self.shared.config.completion_mode {
            CompletionMode::Readiness => {
                let published = self
                    .shared
                    .work_signal
                    .register_and_recheck(cx.waker(), observed_epoch);
                self.mark_published_work(published);
                if self.scheduler.ready_class_count() > 0 {
                    cx.waker().wake_by_ref();
                }
            }
            CompletionMode::Polling => {
                self.shared
                    .driver_yields
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                cx.waker().wake_by_ref();
            }
        }
        Poll::Pending
    }
}

impl Drop for RdmaEngineDriver {
    fn drop(&mut self) {
        self.release_resources();
        if self.shared.outcome().is_none() {
            #[cfg(any(test, feature = "test-hooks"))]
            let failure = {
                let outstanding = self.shared.test_driver.accepted_outstanding();
                if outstanding == 0 {
                    EngineFailure::DriverShutdown
                } else {
                    EngineFailure::Wedged {
                        retained_bundles: self.shared.test_driver.route_count(),
                        outstanding_operations: outstanding,
                    }
                }
            };
            #[cfg(not(any(test, feature = "test-hooks")))]
            let failure = EngineFailure::DriverShutdown;
            self.shared.finish(EngineOutcome::Failure(failure));
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
pub(super) mod test_api {
    use std::any::Any;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, Weak};

    use tokio::sync::Notify;

    use crate::cm::CmId;
    use crate::v2::engine::resources::TestResourceRefs;
    use crate::v2::{AccessIntent, Mr, Qp, QpBuilder};
    use crate::wc::{WcOpcode, WorkCompletion};

    use super::{EngineShared, Error, READY_CONNECTION_WORK, Result};

    /// Test-only connection identity used by the Phase 2 routing gate.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TestConnectionIdentity {
        pub slot: u32,
        pub generation: u32,
        pub qp_num: u32,
    }

    /// Test-only accepted operation installed in the engine route table.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TestAcceptedOperation {
        pub wr_id: u64,
        pub expected_opcode: WcOpcode,
    }

    impl TestAcceptedOperation {
        pub fn new(wr_id: u64, expected_opcode: WcOpcode) -> Self {
            Self {
                wr_id,
                expected_opcode,
            }
        }
    }

    /// Safe test-only lease for the engine's exact context, PD, and shared CQ.
    ///
    /// The lease exposes only MR registration and verified QP construction. It
    /// does not expose raw pointers, file descriptors, CQ polling, or mutable
    /// production engine state.
    #[doc(hidden)]
    #[derive(Clone)]
    pub struct TestEngineResources {
        shared: Weak<EngineShared>,
        resources: TestResourceRefs,
    }

    /// Non-clonable test QP that must be consumed by route installation.
    #[doc(hidden)]
    pub struct TestEngineQp {
        qp: Qp,
    }

    impl TestEngineResources {
        pub(in crate::v2::engine) fn new(
            shared: &Arc<EngineShared>,
            resources: TestResourceRefs,
        ) -> Self {
            Self {
                shared: Arc::downgrade(shared),
                resources,
            }
        }

        /// Verify that a CM route selected the engine's exact anchored context.
        pub fn require_context(&self, cm_id: &CmId) -> Result<()> {
            cm_id
                .require_context(self.resources.context.inner())
                .map_err(Error::from)
        }

        /// Register owned test memory through the engine's shared PD.
        pub fn register_memory(&self, len: usize, access: AccessIntent) -> Result<Mr> {
            self.ensure_active()?;
            self.resources.pd.reg_mr(len, access)
        }

        /// Create a test QP against the engine's exact shared PD and CQ.
        pub fn create_qp(
            &self,
            cm_id: &CmId,
            max_send_wr: u32,
            max_recv_wr: u32,
        ) -> Result<TestEngineQp> {
            self.ensure_active()?;
            self.require_context(cm_id)?;
            Ok(TestEngineQp {
                qp: QpBuilder::new(&self.resources.pd, &self.resources.cq, &self.resources.cq)
                    .max_send_wr(max_send_wr)
                    .max_recv_wr(max_recv_wr)
                    .build_with_cm(cm_id)?,
            })
        }

        /// Install a verified test-only route and its exact accepted set.
        pub fn install_route(
            &self,
            qp: TestEngineQp,
            operations: impl IntoIterator<Item = TestAcceptedOperation>,
        ) -> Result<TestRouteHandle> {
            let shared = self.ensure_active()?;
            if !qp.qp.uses_resources(&self.resources.pd, &self.resources.cq) {
                return Err(Error::InvalidConfig(
                    "test QP was not created from the leased engine PD/shared CQ".into(),
                ));
            }
            shared
                .test_driver
                .install(&shared, self.resources.clone(), Arc::new(qp.qp), operations)
        }

        fn ensure_active(&self) -> Result<Arc<EngineShared>> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            if shared.outcome().is_some() || shared.shutdown_requested.load(Ordering::Acquire) {
                return Err(Error::DriverShutdown);
            }
            Ok(shared)
        }

        /// Current successful CQ-arm generation, for coordinated race tests.
        pub fn cq_arm_generation(&self) -> u64 {
            self.shared.upgrade().map_or(0, |shared| {
                shared.test_driver.cq_arms.load(Ordering::Acquire)
            })
        }

        /// Wait until the readiness driver completes a CQ arm after `previous`.
        pub async fn wait_for_cq_arm_after(&self, previous: u64) -> Result<u64> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            loop {
                let notified = shared.test_driver.cq_arm_notify.notified();
                let current = shared.test_driver.cq_arms.load(Ordering::Acquire);
                if current > previous {
                    return Ok(current);
                }
                notified.await;
            }
        }
    }

    /// Take-once handle for one test-only route.
    #[doc(hidden)]
    pub struct TestRouteHandle {
        shared: Weak<EngineShared>,
        route: Arc<TestRouteState>,
        removed: bool,
    }

    impl TestRouteHandle {
        pub fn identity(&self) -> TestConnectionIdentity {
            self.route.identity
        }

        pub fn qp(&self) -> &Qp {
            &self.route.qp
        }

        pub fn accepted_outstanding(&self) -> usize {
            self.route.remaining()
        }

        /// Retain a posted MR, CM owner, or other dependency with this route.
        pub fn retain<T: Send + 'static>(&self, value: T) {
            self.route
                .retained
                .lock()
                .expect("test route retained resources poisoned")
                .push(Box::new(value));
        }

        pub async fn wait_until_drained(&self) {
            loop {
                let notified = self.route.drained.notified();
                if self.route.remaining() == 0 {
                    return;
                }
                notified.await;
            }
        }

        pub fn completions(&self) -> Vec<WorkCompletion> {
            self.route
                .completions
                .lock()
                .expect("test route completions poisoned")
                .clone()
        }

        pub async fn wait_for_completion_count(&self, expected: usize) {
            loop {
                let notified = self.route.drained.notified();
                if self
                    .route
                    .completions
                    .lock()
                    .expect("test route completions poisoned")
                    .len()
                    >= expected
                {
                    return;
                }
                notified.await;
            }
        }

        /// Arm deterministic suppression of the next exact CQE for `wr_id`.
        pub fn suppress_next(&self, wr_id: u64) -> Result<TestCqeSuppression> {
            self.route.arm_suppression(wr_id)?;
            Ok(TestCqeSuppression {
                shared: self.shared.clone(),
                route: Arc::clone(&self.route),
                wr_id,
            })
        }

        /// Remove this route after its exact accepted set reaches zero.
        pub fn remove(mut self) -> Result<Vec<WorkCompletion>> {
            if self.route.remaining() != 0 {
                return Err(Error::InvalidConfig(
                    "cannot remove a test route with accepted WRs outstanding".into(),
                ));
            }
            if let Some(shared) = self.shared.upgrade() {
                shared
                    .test_driver
                    .remove(self.route.identity.qp_num, &self.route);
            }
            self.removed = true;
            Ok(self.completions())
        }
    }

    impl Drop for TestRouteHandle {
        fn drop(&mut self) {
            if self.removed {
                return;
            }
            self.route.detached.store(true, Ordering::Release);
            if let Some(shared) = self.shared.upgrade() {
                if self.route.remaining() == 0 {
                    shared
                        .test_driver
                        .remove(self.route.identity.qp_num, &self.route);
                }
            } else if self.route.remaining() != 0 {
                quarantine_routes()
                    .lock()
                    .expect("test route quarantine poisoned")
                    .push(Arc::clone(&self.route));
            }
        }
    }

    /// Armed deterministic CQE suppression fixture.
    #[doc(hidden)]
    pub struct TestCqeSuppression {
        shared: Weak<EngineShared>,
        route: Arc<TestRouteState>,
        wr_id: u64,
    }

    impl TestCqeSuppression {
        pub async fn wait_observed(&self) {
            loop {
                let notified = self.route.suppression_observed.notified();
                if self
                    .route
                    .suppressed_completions
                    .lock()
                    .expect("test route suppressed completions poisoned")
                    .contains_key(&self.wr_id)
                {
                    return;
                }
                notified.await;
            }
        }

        /// Release the recorded CQE back into normal exact-route processing.
        pub fn release(self) -> Result<()> {
            let completion = self
                .route
                .suppressed_completions
                .lock()
                .expect("test route suppressed completions poisoned")
                .remove(&self.wr_id)
                .ok_or_else(|| {
                    Error::InvalidConfig(format!(
                        "suppressed operation token {} has not been observed",
                        self.wr_id
                    ))
                })?;
            let drained = self.route.accept_completion(completion);
            if drained
                && self.route.detached.load(Ordering::Acquire)
                && let Some(shared) = self.shared.upgrade()
            {
                shared
                    .test_driver
                    .remove(self.route.identity.qp_num, &self.route);
            }
            Ok(())
        }
    }

    pub(in crate::v2::engine) struct TestDriverState {
        routes: Mutex<HashMap<u32, Arc<TestRouteState>>>,
        next_slot: AtomicU32,
        cq_arms: AtomicU64,
        cq_arm_notify: Notify,
    }

    impl TestDriverState {
        pub(in crate::v2::engine) fn new() -> Self {
            Self {
                routes: Mutex::new(HashMap::new()),
                next_slot: AtomicU32::new(0),
                cq_arms: AtomicU64::new(0),
                cq_arm_notify: Notify::new(),
            }
        }

        fn install(
            &self,
            shared: &Arc<EngineShared>,
            resources: TestResourceRefs,
            qp: Arc<Qp>,
            operations: impl IntoIterator<Item = TestAcceptedOperation>,
        ) -> Result<TestRouteHandle> {
            let qp_num = qp.qp_num();
            if qp_num == 0 {
                return Err(Error::InvalidConfig(
                    "provider returned zero qp_num for test route".into(),
                ));
            }
            let mut accepted = HashMap::new();
            for operation in operations {
                if accepted
                    .insert(operation.wr_id, operation.expected_opcode)
                    .is_some()
                {
                    return Err(Error::InvalidConfig(format!(
                        "duplicate test operation token {}",
                        operation.wr_id
                    )));
                }
            }
            let slot = self
                .next_slot
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |slot| {
                    slot.checked_add(1)
                })
                .map_err(|_| Error::CapacityExhausted)?;
            let route = Arc::new(TestRouteState {
                identity: TestConnectionIdentity {
                    slot,
                    generation: 1,
                    qp_num,
                },
                qp,
                retained: Mutex::new(Vec::new()),
                resources,
                accepted: Mutex::new(accepted),
                completions: Mutex::new(Vec::new()),
                suppressed: Mutex::new(HashSet::new()),
                suppressed_completions: Mutex::new(HashMap::new()),
                drained: Notify::new(),
                suppression_observed: Notify::new(),
                detached: AtomicBool::new(false),
            });
            let mut routes = self.routes.lock().expect("test route table poisoned");
            if routes.contains_key(&qp_num) {
                return Err(Error::InvalidConfig(format!(
                    "test route already installed for qp_num {qp_num}"
                )));
            }
            routes.insert(qp_num, Arc::clone(&route));
            drop(routes);
            shared.work_signal.publish(READY_CONNECTION_WORK);
            Ok(TestRouteHandle {
                shared: Arc::downgrade(shared),
                route,
                removed: false,
            })
        }

        pub(super) fn dispatch(&self, completion: WorkCompletion) {
            let route = self
                .routes
                .lock()
                .expect("test route table poisoned")
                .get(&completion.qp_num())
                .cloned();
            let Some(route) = route else {
                return;
            };
            let drained = route.complete(completion);
            if drained && route.detached.load(Ordering::Acquire) {
                self.remove(completion.qp_num(), &route);
            }
        }

        fn remove(&self, qp_num: u32, route: &Arc<TestRouteState>) {
            let mut routes = self.routes.lock().expect("test route table poisoned");
            if routes
                .get(&qp_num)
                .is_some_and(|current| Arc::ptr_eq(current, route))
            {
                routes.remove(&qp_num);
            }
        }

        pub(in crate::v2::engine) fn accepted_outstanding(&self) -> usize {
            let routes = self.routes.lock().expect("test route table poisoned");
            routes.values().map(|route| route.remaining()).sum()
        }

        pub(super) fn route_count(&self) -> usize {
            self.routes.lock().expect("test route table poisoned").len()
        }

        pub(super) fn record_cq_arms(&self, count: u64) {
            self.cq_arms.fetch_add(count, Ordering::AcqRel);
            self.cq_arm_notify.notify_waiters();
        }

        pub(super) fn retain_unresolved(&self) {
            let unresolved: Vec<_> = self
                .routes
                .lock()
                .expect("test route table poisoned")
                .values()
                .filter(|route| route.remaining() != 0)
                .cloned()
                .collect();
            if unresolved.is_empty() {
                return;
            }
            quarantine_routes()
                .lock()
                .expect("test route quarantine poisoned")
                .extend(unresolved);
        }
    }

    struct TestRouteState {
        identity: TestConnectionIdentity,
        qp: Arc<Qp>,
        retained: Mutex<Vec<Box<dyn Any + Send>>>,
        #[allow(
            dead_code,
            reason = "retains the engine CQ channel, PD, and anchored context with quarantined routes"
        )]
        resources: TestResourceRefs,
        accepted: Mutex<HashMap<u64, WcOpcode>>,
        completions: Mutex<Vec<WorkCompletion>>,
        suppressed: Mutex<HashSet<u64>>,
        suppressed_completions: Mutex<HashMap<u64, WorkCompletion>>,
        drained: Notify,
        suppression_observed: Notify,
        detached: AtomicBool,
    }

    impl TestRouteState {
        fn remaining(&self) -> usize {
            self.accepted
                .lock()
                .expect("test route accepted set poisoned")
                .len()
        }

        fn arm_suppression(&self, wr_id: u64) -> Result<()> {
            if !self
                .accepted
                .lock()
                .expect("test route accepted set poisoned")
                .contains_key(&wr_id)
            {
                return Err(Error::InvalidConfig(format!(
                    "cannot suppress unknown operation token {wr_id}"
                )));
            }
            let mut suppressed = self
                .suppressed
                .lock()
                .expect("test route suppression set poisoned");
            if !suppressed.insert(wr_id) {
                return Err(Error::InvalidConfig(format!(
                    "operation token {wr_id} is already armed for suppression"
                )));
            }
            Ok(())
        }

        fn complete(&self, completion: WorkCompletion) -> bool {
            let wr_id = completion.wr_id();
            let suppressed = self
                .suppressed
                .lock()
                .expect("test route suppression set poisoned")
                .remove(&wr_id);
            if suppressed {
                self.suppressed_completions
                    .lock()
                    .expect("test route suppressed completions poisoned")
                    .insert(wr_id, completion);
                self.suppression_observed.notify_waiters();
                return false;
            }

            self.accept_completion(completion)
        }

        fn accept_completion(&self, completion: WorkCompletion) -> bool {
            let wr_id = completion.wr_id();
            let removed = {
                let mut accepted = self
                    .accepted
                    .lock()
                    .expect("test route accepted set poisoned");
                match accepted.get(&wr_id) {
                    Some(expected)
                        // Providers may leave the opcode field unspecified on
                        // error/flush CQEs; exact token and QP identity remain
                        // mandatory, while successful CQEs also match opcode.
                        if !completion.is_success() || *expected == completion.opcode() =>
                    {
                        accepted.remove(&wr_id);
                        true
                    }
                    _ => false,
                }
            };
            if !removed {
                return false;
            }
            self.completions
                .lock()
                .expect("test route completions poisoned")
                .push(completion);
            let drained = self.remaining() == 0;
            self.drained.notify_waiters();
            drained
        }
    }

    fn quarantine_routes() -> &'static Mutex<Vec<Arc<TestRouteState>>> {
        static ROUTES: OnceLock<Mutex<Vec<Arc<TestRouteState>>>> = OnceLock::new();
        ROUTES.get_or_init(|| Mutex::new(Vec::new()))
    }

    impl Drop for EngineShared {
        fn drop(&mut self) {
            self.test_driver.retain_unresolved();
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
pub use test_api::{
    TestAcceptedOperation, TestConnectionIdentity, TestCqeSuppression, TestEngineQp,
    TestEngineResources, TestRouteHandle,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{RawWaker, RawWakerVTable, Waker};

    use super::*;
    use crate::v2::engine::{RdmaEngineLifecycle, test_engine_pair};

    struct CountingWaker(AtomicUsize);

    impl CountingWaker {
        fn new() -> Arc<Self> {
            Arc::new(Self(AtomicUsize::new(0)))
        }

        fn count(&self) -> usize {
            self.0.load(Ordering::Acquire)
        }

        fn waker(self: &Arc<Self>) -> Waker {
            unsafe fn clone(ptr: *const ()) -> RawWaker {
                let value = unsafe { Arc::from_raw(ptr.cast::<CountingWaker>()) };
                let cloned = Arc::clone(&value);
                std::mem::forget(value);
                RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
            }
            unsafe fn wake(ptr: *const ()) {
                let value = unsafe { Arc::from_raw(ptr.cast::<CountingWaker>()) };
                value.0.fetch_add(1, Ordering::AcqRel);
            }
            unsafe fn wake_by_ref(ptr: *const ()) {
                let value = unsafe { Arc::from_raw(ptr.cast::<CountingWaker>()) };
                value.0.fetch_add(1, Ordering::AcqRel);
                std::mem::forget(value);
            }
            unsafe fn drop_waker(ptr: *const ()) {
                unsafe { drop(Arc::from_raw(ptr.cast::<CountingWaker>())) };
            }
            static VTABLE: RawWakerVTable =
                RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
            let raw = RawWaker::new(Arc::into_raw(Arc::clone(self)).cast(), &VTABLE);
            unsafe { Waker::from_raw(raw) }
        }
    }

    #[test]
    fn wake_before_register_is_seen_by_recheck() {
        let signal = WorkSignal::new();
        signal.publish(TERMINAL_WORK);
        let observed = signal.epoch();
        let counter = CountingWaker::new();
        let pending = signal.register_and_recheck(&counter.waker(), observed - 1);
        assert_eq!(pending, TERMINAL_WORK);
        assert_eq!(counter.count(), 1);
    }

    #[test]
    fn wake_during_register_is_not_lost() {
        let signal = Arc::new(WorkSignal::new());
        let observed = signal.epoch();
        std::thread::scope(|scope| {
            let signal = Arc::clone(&signal);
            scope
                .spawn(move || signal.publish(RECLAMATION_WORK))
                .join()
                .unwrap();
        });
        let counter = CountingWaker::new();
        let pending = signal.register_and_recheck(&counter.waker(), observed);
        assert_eq!(pending, RECLAMATION_WORK);
        assert_eq!(counter.count(), 1);
    }

    #[test]
    fn enqueue_after_drain_is_seen_by_register_recheck() {
        let signal = WorkSignal::new();
        assert_eq!(signal.take(), 0);
        let observed = signal.epoch();
        signal.publish(READY_CONNECTION_WORK);
        let counter = CountingWaker::new();
        assert_eq!(
            signal.register_and_recheck(&counter.waker(), observed),
            READY_CONNECTION_WORK
        );
        assert_eq!(counter.count(), 1);
    }

    #[test]
    fn concurrent_producers_coalesce_without_losing_work_classes() {
        let signal = Arc::new(WorkSignal::new());
        std::thread::scope(|scope| {
            let mut producers = Vec::new();
            for bit in [TERMINAL_WORK, RECLAMATION_WORK, READY_CONNECTION_WORK] {
                let signal = Arc::clone(&signal);
                producers.push(scope.spawn(move || {
                    for _ in 0..32 {
                        signal.publish(bit);
                    }
                }));
            }
            for producer in producers {
                producer.join().unwrap();
            }
        });
        assert_eq!(
            signal.take(),
            TERMINAL_WORK | RECLAMATION_WORK | READY_CONNECTION_WORK
        );
    }

    #[tokio::test]
    async fn readiness_idle_poll_does_not_self_wake_or_scan() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert_eq!(counter.count(), 0);
        assert_eq!(driver.scheduler.ready_connection_count(), 0);
    }

    #[tokio::test]
    async fn polling_empty_iteration_cooperatively_yields_once() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert_eq!(counter.count(), 1);
        assert_eq!(
            driver.shared.driver_yields.load(Ordering::Acquire),
            1,
            "one empty polling iteration records one cooperative yield"
        );
    }

    #[tokio::test]
    async fn terminal_request_wakes_driver_and_state_is_monotonic() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        engine.shared.request_shutdown();
        assert_eq!(counter.count(), 1);
        assert!(matches!(
            Pin::new(&mut driver).poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        engine.shared.transition_running();
        assert_eq!(engine.shared.lifecycle(), RdmaEngineLifecycle::Terminated);
    }
}
