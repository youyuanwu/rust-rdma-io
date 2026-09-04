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

use super::config::CompletionMode;
use super::lifecycle::MemoizedTerminalResult;
use super::resources::EngineResources;
use super::scheduler::{Deadline, DeadlineKind, WorkClass, WorkScheduler};
use super::{EngineShared, RdmaEngineDriver};
use crate::v2::Completion;
use crate::v2::completion::CqReadiness;
use crate::v2::error::{Error, Result};
use crate::v2::runtime::preflight_driver_runtime;

pub(super) const TERMINAL_WORK: usize = 1 << 0;
pub(super) const RECLAMATION_WORK: usize = 1 << 1;
pub(super) const COMPLETION_DISPATCH_WORK: usize = 1 << 2;
pub(super) const CQ_RECHECK_WORK: usize = 1 << 4;
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

fn poll_readiness_events<G>(
    cx: &mut TaskContext<'_>,
    budget: usize,
    mut poll_read_ready: impl FnMut(&mut TaskContext<'_>) -> Poll<Result<G>>,
    mut clear_ready: impl FnMut(&mut G),
    mut try_one: impl FnMut() -> Result<bool>,
) -> Poll<Result<usize>> {
    debug_assert!(budget > 0);
    let mut processed = 0;
    loop {
        let mut guard = match poll_read_ready(cx) {
            Poll::Ready(Ok(guard)) => guard,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending if processed == 0 => return Poll::Pending,
            Poll::Pending => return Poll::Ready(Ok(processed)),
        };
        loop {
            if processed == budget {
                return Poll::Ready(Ok(processed));
            }
            match try_one() {
                Ok(true) => processed += 1,
                Ok(false) => {
                    clear_ready(&mut guard);
                    break;
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        // Clearing an AsyncFd readiness tick can race a new edge. Re-polling
        // here either observes it immediately or registers the task waker
        // before this driver poll is allowed to return Pending.
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
            cq_buffer: vec![Completion::default(); cq_budget].into_boxed_slice(),
            deadline_sleep: None,
            deadline_at: None,
            deadline_io_turn: true,
            runtime_checked: false,
        }
    }

    fn mark_published_work(&mut self, published: usize) {
        if published & TERMINAL_WORK != 0 {
            self.scheduler.mark_class_ready(WorkClass::Terminal);
        }
        if published & RECLAMATION_WORK != 0 {
            self.scheduler.mark_class_ready(WorkClass::Reclamation);
        }
        if published & COMPLETION_DISPATCH_WORK != 0 {
            self.scheduler
                .mark_class_ready(WorkClass::CompletionDispatch);
        }
        if published & super::cm::CM_WORK != 0 {
            self.scheduler.mark_class_ready(WorkClass::Cm);
        }
        if published & CQ_RECHECK_WORK != 0 {
            self.scheduler.mark_class_ready(WorkClass::Cq);
        }
    }

    fn fail(&mut self, error: Error) -> Poll<Result<()>> {
        let outcome = MemoizedTerminalResult::from_error(error);
        self.shared.finish(outcome.clone());
        EngineShared::retain_after_failure(&self.shared);
        self.release_resources();
        Poll::Ready(outcome.into_result())
    }

    fn release_resources(&mut self) {
        if let Some(resources) = self.resources.as_mut() {
            resources.drop_readiness_adapters();
        }
        self.resources.take();
    }

    fn service_terminal(&mut self) -> Result<bool> {
        if !self
            .shared
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(false);
        }
        self.shared.cm.begin_shutdown(
            &self.shared,
            &MemoizedTerminalResult::from_error(Error::DriverShutdown),
        );
        self.shared.begin_all_connection_close();
        if self.shared.cm.pending_route_count() != 0 {
            return Ok(false);
        }
        if self.shared.connections.live() != 0 {
            return Ok(false);
        }

        if self.shared.io_core.accepted_count() != 0 {
            return Ok(false);
        }

        if let Some(resources) = self.resources.as_ref() {
            let mut processed = 0;
            while processed < self.shared.config.cm_event_budget {
                if !self.shared.cm.try_process_event(&self.shared, resources)? {
                    if self.shared.cm.pending_route_count() != 0
                        || self.shared.connections.live() != 0
                        || self.shared.io_core.accepted_count() != 0
                    {
                        self.scheduler.mark_class_ready(WorkClass::Cm);
                        return Ok(false);
                    }
                    #[cfg(any(test, feature = "test-hooks"))]
                    crate::test_support::destruction::record(
                        crate::test_support::destruction::DestructionKind::CmFinalDrainToWouldBlock,
                        resources.cm_event_channel.as_raw() as usize,
                    );
                    self.shared.finish(MemoizedTerminalResult::success());
                    return Ok(true);
                }
                processed += 1;
            }
            self.scheduler.mark_class_ready(WorkClass::Cm);
            return Ok(false);
        }

        self.shared.finish(MemoizedTerminalResult::success());
        Ok(true)
    }

    fn service_cq(&mut self, cx: &mut TaskContext<'_>) -> Result<bool> {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(completion) = self.shared.test_driver.take_released_connection_cqe() {
            if let Some(connection) = self.shared.enqueue_completion(completion) {
                self.scheduler
                    .enqueue_connection(connection.completion_ready());
            }
            self.scheduler.mark_class_ready(WorkClass::Cq);
            return Ok(true);
        }

        let Some(resources) = self.resources.as_ref() else {
            return Ok(false);
        };
        let count = match self.shared.config.completion_mode {
            CompletionMode::Readiness => {
                let async_fd = resources.cq_async_fd.as_ref().ok_or_else(|| {
                    Error::InvalidConfig("readiness engine has no CQ AsyncFd".into())
                })?;
                #[cfg(any(test, feature = "test-hooks"))]
                let polled = {
                    let before = Arc::clone(&self.shared);
                    let after = Arc::clone(&self.shared);
                    self.cq_readiness.poll_with_async_fd_and_hooks(
                        &resources.cq,
                        async_fd,
                        cx,
                        &mut self.cq_buffer,
                        move |generation| before.test_driver.record_cq_pre_arm(generation),
                        move |generation| after.test_driver.record_cq_arm(generation),
                    )
                };
                #[cfg(not(any(test, feature = "test-hooks")))]
                let polled = self.cq_readiness.poll_with_async_fd_and_hooks(
                    &resources.cq,
                    async_fd,
                    cx,
                    &mut self.cq_buffer,
                    |_| false,
                    |_| false,
                );
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
            let completion = completion.into_raw();
            #[cfg(any(test, feature = "test-hooks"))]
            if self.shared.test_driver.suppress_connection_cqe(completion) {
                continue;
            }
            if let Some(connection) = self.shared.enqueue_completion(completion) {
                self.scheduler
                    .enqueue_connection(connection.completion_ready());
            }
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
        let budget = self.shared.config.cm_event_budget;
        let cm = &self.shared.cm;
        let mut processed = cm.service_software(&self.shared, Some(resources), budget)?;
        while processed < budget && self.shared.cm.try_process_event(&self.shared, resources)? {
            processed += 1;
        }

        let readiness_processed = match self.shared.config.completion_mode {
            CompletionMode::Readiness => {
                if processed == budget {
                    0
                } else {
                    let async_fd = resources.cm_async_fd.as_ref().ok_or_else(|| {
                        Error::InvalidConfig("readiness engine has no CM AsyncFd".into())
                    })?;
                    match poll_readiness_events(
                        cx,
                        budget - processed,
                        |cx| match async_fd.poll_read_ready(cx) {
                            Poll::Ready(Ok(guard)) => Poll::Ready(Ok(guard)),
                            Poll::Ready(Err(error)) => Poll::Ready(Err(Error::Verbs(error))),
                            Poll::Pending => Poll::Pending,
                        },
                        |guard| guard.clear_ready(),
                        || self.shared.cm.try_process_event(&self.shared, resources),
                    ) {
                        Poll::Ready(result) => result?,
                        Poll::Pending => 0,
                    }
                }
            }
            CompletionMode::Polling => 0,
        };
        processed += readiness_processed;
        processed +=
            cm.service_cm_destructions(&self.shared, budget.saturating_sub(processed), || {
                cm.try_process_event(&self.shared, resources)
            })?;

        if processed >= budget || self.shared.cm.has_software_work() {
            self.scheduler.mark_class_ready(WorkClass::Cm);
        }
        Ok(processed > 0)
    }

    fn service_reclamation(&mut self) -> Result<bool> {
        let budget = self.shared.config.reclamation_budget;
        let first_quota = budget.div_ceil(2);
        let second_quota = budget / 2;
        let mut requests = if self.deadline_io_turn {
            self.shared.io_core.take_reclamation_requests(first_quota)
        } else {
            self.shared.take_deadline_requests(first_quota)
        };
        let second = if self.deadline_io_turn {
            self.shared.take_deadline_requests(second_quota)
        } else {
            self.shared.io_core.take_reclamation_requests(second_quota)
        };
        requests.extend(second);
        if requests.len() < budget {
            let remaining = budget - requests.len();
            let refill = if self.deadline_io_turn {
                self.shared.io_core.take_reclamation_requests(remaining)
            } else {
                self.shared.take_deadline_requests(remaining)
            };
            requests.extend(refill);
        }
        if requests.len() < budget {
            let remaining = budget - requests.len();
            let refill = if self.deadline_io_turn {
                self.shared.take_deadline_requests(remaining)
            } else {
                self.shared.io_core.take_reclamation_requests(remaining)
            };
            requests.extend(refill);
        }
        self.deadline_io_turn = !self.deadline_io_turn;
        for request in requests.iter().copied() {
            if !self
                .scheduler
                .deadlines()
                .push(request.at, request.kind, request.token)
            {
                return Err(Error::InvalidConfig(
                    "deadline insertion sequence exhausted".into(),
                ));
            }
        }
        let now = tokio::time::Instant::now();
        let remaining = budget.saturating_sub(requests.len());
        let due = self.scheduler.deadlines().pop_due(now, remaining);
        for deadline in due.iter().copied() {
            self.process_deadline(deadline)?;
        }
        if self.shared.has_deadline_requests()
            || self.shared.io_core.has_reclamation_requests()
            || self
                .scheduler
                .deadlines()
                .next()
                .is_some_and(|at| at <= now)
        {
            self.scheduler.mark_class_ready(WorkClass::Reclamation);
        }
        Ok(!due.is_empty() || !requests.is_empty())
    }

    fn process_deadline(&mut self, deadline: Deadline) -> Result<()> {
        match deadline.kind {
            DeadlineKind::EngineShutdown => {
                if let Some(failure) = self.shared.shutdown_deadline_failure() {
                    Err(failure)
                } else {
                    self.scheduler.mark_class_ready(WorkClass::Terminal);
                    Ok(())
                }
            }
            DeadlineKind::Reclamation => {
                self.shared
                    .handle_reclamation_deadline(super::registry::OperationToken::decode(
                        deadline.token,
                    ));
                Ok(())
            }
            DeadlineKind::ConnectionDrain => {
                self.shared.handle_connection_drain_deadline(
                    super::registry::ConnectionToken::decode(deadline.token),
                );
                Ok(())
            }
        }
    }

    fn poll_deadline_timer(&mut self, cx: &mut TaskContext<'_>) -> bool {
        let next = self.scheduler.next_deadline();
        if self.deadline_at != next {
            self.deadline_sleep = next.map(|at| Box::pin(tokio::time::sleep_until(at)));
            self.deadline_at = next;
        }
        let Some(sleep) = self.deadline_sleep.as_mut() else {
            return false;
        };
        if sleep.as_mut().poll(cx).is_pending() {
            return false;
        }
        self.deadline_sleep = None;
        self.deadline_at = None;
        self.scheduler.mark_class_ready(WorkClass::Reclamation);
        true
    }

    fn service_completion_dispatch(&mut self) -> bool {
        if let Some(connection) = self.shared.take_published_completion() {
            self.scheduler
                .enqueue_connection(connection.completion_ready());
        }
        let Some(connection) = self.scheduler.pop_connection() else {
            return false;
        };
        let (_, remains_ready) = self.shared.dispatch_connection_completions(
            super::registry::ConnectionToken {
                slot: connection.slot,
                generation: connection.generation,
            },
            self.shared.config.completion_dispatch_budget,
        );
        if remains_ready {
            self.scheduler.requeue_connection(connection);
        }
        if self.scheduler.completion_connection_count() > 0 {
            self.scheduler
                .mark_class_ready(WorkClass::CompletionDispatch);
        }
        if self.shared.has_published_completions() {
            self.scheduler
                .mark_class_ready(WorkClass::CompletionDispatch);
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

        if !self.runtime_checked {
            if let Err(error) = preflight_driver_runtime("RdmaEngineDriver") {
                return self.fail(error);
            }
            self.runtime_checked = true;
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(error) = self.shared.test_driver.take_injected_failure() {
            return self.fail(error);
        }
        self.shared.transition_running();
        self.poll_deadline_timer(cx);

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
                WorkClass::Terminal => match self.service_terminal() {
                    Ok(true) => break,
                    Ok(false) => Ok(false),
                    Err(error) => Err(error),
                },
                WorkClass::Cm => self.service_cm(cx),
                WorkClass::Cq => self.service_cq(cx),
                WorkClass::Reclamation => self.service_reclamation(),
                WorkClass::CompletionDispatch => Ok(self.service_completion_dispatch()),
            };
            let progressed = match result {
                Ok(progressed) => progressed,
                Err(error) => return self.fail(error),
            };
            // CM progress can remove the final shutdown owner after the
            // Terminal class already ran in this poll.
            if progressed
                && class == WorkClass::Cm
                && self
                    .shared
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                match self.service_terminal() {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => return self.fail(error),
                }
            }
            if self.shared.outcome().is_some() {
                break;
            }
        }

        if let Some(outcome) = self.shared.outcome() {
            self.release_resources();
            return Poll::Ready(outcome.into_result());
        }

        if self.poll_deadline_timer(cx) {
            cx.waker().wake_by_ref();
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
                // Tokio's yield future defers this task to the back of the
                // scheduler queue. A direct self-wake can monopolize a worker
                // and starve the explicit per-connection message drivers.
                let mut yield_now = std::pin::pin!(tokio::task::yield_now());
                if yield_now.as_mut().poll(cx).is_ready() {
                    cx.waker().wake_by_ref();
                }
            }
        }
        Poll::Pending
    }
}

impl Drop for RdmaEngineDriver {
    fn drop(&mut self) {
        if self.shared.outcome().is_none() {
            self.shared.mark_shutdown_requested();
            self.shared.synchronously_prepare_driver_drop();
            let outstanding = self.shared.io_core.accepted_count();
            let cm_owners = self
                .shared
                .cm
                .retained_owner_count()
                .max(self.shared.connections.live());
            let error = if outstanding == 0 && cm_owners == 0 {
                Error::DriverShutdown
            } else {
                Error::EngineWedged {
                    retained_bundles: self.shared.retained_bundle_count().max(1),
                    outstanding_operations: outstanding,
                    cq_debt: outstanding,
                }
            };
            self.shared
                .finish(MemoizedTerminalResult::from_error(error));
            EngineShared::retain_after_failure(&self.shared);
        }
        self.release_resources();
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub(super) mod test_api {
    use std::any::Any;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
    use std::time::Duration;

    use tokio::sync::Notify;

    use crate::async_cm::AsyncCmId;
    use crate::cm::CmId;
    #[cfg(test)]
    use crate::v2::engine::connection::WorkRequestPoster;
    use crate::v2::engine::connection::{VerbsConnectionResources, install_connection};
    use crate::v2::engine::resources::TestResourceRefs;
    #[cfg(test)]
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::v2::{
        AccessIntent, Completion, Mr, Qp, QpBuilder, RdmaConnection, RdmaConnectionConfig,
    };
    use crate::wc::{WcOpcode, WcStatus, WorkCompletion};
    #[cfg(test)]
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    use super::{COMPLETION_DISPATCH_WORK, EngineShared, Error, Result, TERMINAL_WORK};
    use crate::v2::engine::io_core::CqeReject;
    use crate::v2::engine::registry::{Lookup, OperationToken};

    /// Test-only connection identity used by the Phase 2 routing gate.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct RouteIdentity {
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

    /// Exact class of a CQE rejected before it could mutate live ownership.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TestCqeRejection {
        StaleConnection,
        StaleOperation,
        RetiredOperation,
        Unknown,
        Duplicate,
        WrongConnection,
        WrongQpNum,
        UnexpectedOpcode,
    }

    impl From<CqeReject> for TestCqeRejection {
        fn from(value: CqeReject) -> Self {
            match value {
                CqeReject::StaleConnection => Self::StaleConnection,
                CqeReject::StaleOperation => Self::StaleOperation,
                CqeReject::RetiredOperation => Self::RetiredOperation,
                CqeReject::Unknown => Self::Unknown,
                CqeReject::Duplicate => Self::Duplicate,
                CqeReject::WrongConnection => Self::WrongConnection,
                CqeReject::WrongQpNum => Self::WrongQpNum,
                CqeReject::UnexpectedOpcode => Self::UnexpectedOpcode,
            }
        }
    }

    impl TestAcceptedOperation {
        pub fn new(wr_id: u64, expected_opcode: WcOpcode) -> Self {
            Self {
                wr_id,
                expected_opcode,
            }
        }
    }

    /// Minimal routing and CM-ownership observation for safety tests.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct TestEngineInstrumentation {
        /// CM requests and routes that still require driver progress.
        pub cm_pending_routes: usize,
        /// CM routes, listeners, or deferred destructions retaining ownership.
        pub cm_retained_owners: usize,
        /// CQEs rejected before they could affect a live operation.
        pub cqes_rejected: u64,
        /// CM events rejected before they could mutate a live route.
        pub cm_events_rejected: u64,
    }

    /// Safe test-only lease for shared resources and bounded driver fixtures.
    ///
    /// The lease exposes no raw pointers, file descriptors, or CQ consumer. Its
    /// mutation surface is compiled only for deterministic engine validation.
    #[doc(hidden)]
    #[derive(Clone)]
    pub struct TestEngineResources {
        shared: Weak<EngineShared>,
        resources: TestResourceRefs,
    }

    /// Opaque equality-only identity for the engine's anchored context.
    pub struct TestContextIdentity {
        raw_context: usize,
    }

    /// Test-only identity of one engine's shared RDMA resource set.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct TestSharedResourceIdentity {
        context: usize,
        protection_domain: usize,
        completion_queue: usize,
        cm_event_channel: usize,
        completion_channel: Option<usize>,
    }

    /// Read-only provider-capability projection for validation fixtures.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TestProviderLimits {
        max_qp: usize,
        max_qp_wr: usize,
        max_sge: usize,
        max_cqe: usize,
        max_qp_rd_atom: usize,
        max_qp_init_rd_atom: usize,
    }

    /// Controller that pauses the driver after CQ notification arming and
    /// before its mandatory post-arm CQ poll.
    #[doc(hidden)]
    pub struct TestCqArmWindowControl {
        shared: Weak<EngineShared>,
        point: CqArmRacePoint,
        active: bool,
    }

    /// Controller for one exact production connection CQE held after polling.
    #[doc(hidden)]
    pub struct TestConnectionCqeSuppression {
        shared: Weak<EngineShared>,
        connection: super::super::registry::ConnectionToken,
        qp_num: u32,
        active: bool,
    }

    /// Controller for one deterministic engine-admission shutdown race.
    #[doc(hidden)]
    pub struct TestAdmissionBarrier {
        shared: Weak<EngineShared>,
        point: AdmissionPausePoint,
        active: bool,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::v2::engine) enum AdmissionPausePoint {
        ConnectBeforeEnqueue,
        OperationBeforeRegister,
    }

    #[derive(Clone, Copy)]
    enum CqArmRacePoint {
        BeforeArm,
        AfterArm,
    }

    struct AdmissionBarrierState {
        control: Mutex<Option<AdmissionControl>>,
        changed: Condvar,
    }

    struct AdmissionControl {
        point: AdmissionPausePoint,
        paused: bool,
        shutdown_attempted: bool,
        released: bool,
    }

    /// Non-clonable test QP that must be consumed by route installation.
    #[doc(hidden)]
    pub struct TestEngineQp {
        qp: Qp,
    }

    #[cfg(test)]
    struct TestIdlePoster {
        qp_num: u32,
    }

    #[cfg(test)]
    impl WorkRequestPoster for TestIdlePoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            unreachable!("idle registry fixtures never post")
        }

        fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            unreachable!("idle registry fixtures never post")
        }

        fn to_error(&self) -> Result<()> {
            Ok(())
        }

        fn destroy_qp(&self) -> Result<bool> {
            Ok(true)
        }

        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    fn raw_wc_opcode(opcode: WcOpcode) -> Result<u32> {
        match opcode {
            WcOpcode::Send => Ok(rdma_io_sys::ibverbs::IBV_WC_SEND),
            WcOpcode::RdmaWrite => Ok(rdma_io_sys::ibverbs::IBV_WC_RDMA_WRITE),
            WcOpcode::RdmaRead => Ok(rdma_io_sys::ibverbs::IBV_WC_RDMA_READ),
            WcOpcode::Recv => Ok(rdma_io_sys::ibverbs::IBV_WC_RECV),
            _ => Err(Error::InvalidConfig(
                "test completion opcode is not supported".into(),
            )),
        }
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
                .require_context(self.resources.context.raw_context())
                .map_err(Error::from_v1)
        }

        /// Return copied numeric provider limits without exposing validation internals.
        pub fn provider_limits(&self) -> Result<TestProviderLimits> {
            let shared = self.ensure_active()?;
            let limits = shared.provider.ok_or_else(|| {
                Error::InvalidConfig("engine has no provider limits snapshot".into())
            })?;
            Ok(TestProviderLimits {
                max_qp: limits.max_qp,
                max_qp_wr: limits.max_qp_wr,
                max_sge: limits.max_sge,
                max_cqe: limits.max_cqe,
                max_qp_rd_atom: limits.max_qp_rd_atom,
                max_qp_init_rd_atom: limits.max_qp_init_rd_atom,
            })
        }

        /// Return an opaque equality-only identity for the anchored context.
        pub fn context_identity(&self) -> Result<TestContextIdentity> {
            self.ensure_active()?;
            Ok(TestContextIdentity {
                raw_context: self.resources.context.raw_context().as_raw() as usize,
            })
        }

        /// Return identities for the one shared Context/PD/CQ/CM resource set.
        pub fn shared_resource_identity(&self) -> Result<TestSharedResourceIdentity> {
            self.ensure_active()?;
            Ok(TestSharedResourceIdentity {
                context: self.resources.context.raw_context().as_raw() as usize,
                protection_domain: self.resources.pd.raw_pd().as_raw() as usize,
                completion_queue: self.resources.cq.raw_cq().as_raw() as usize,
                cm_event_channel: self.resources.cm_event_channel.as_raw() as usize,
                completion_channel: self
                    .resources
                    .cq
                    .completion_channel()
                    .map(|channel| channel.as_raw() as usize),
            })
        }

        /// Verify that a connection posts through this engine's shared PD/CQ.
        pub fn connection_uses_shared_resources(
            &self,
            connection: &RdmaConnection,
        ) -> Result<bool> {
            let shared = self.ensure_active()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Ok(false);
            }
            Ok(connection
                .state
                .poster
                .uses_resources(&self.resources.pd, &self.resources.cq))
        }

        /// Verify that the connection's exact generational CM route is live.
        pub fn connection_route_is_live(&self, connection: &RdmaConnection) -> Result<bool> {
            let shared = self.ensure_active()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Ok(false);
            }
            let route = connection
                .state
                .cm_route()
                .ok_or_else(|| Error::InvalidConfig("connection has no CM route".into()))?;
            Ok(shared
                .cm
                .connection_route_is_live(route, connection.state.token))
        }

        /// Snapshot the exact rejection classes observed by CQE routing.
        pub fn cqe_rejections(&self) -> Result<Vec<TestCqeRejection>> {
            let shared = self.ensure_active()?;
            Ok(shared
                .io_core
                .rejected_cqe_reasons()
                .iter()
                .copied()
                .map(TestCqeRejection::from)
                .collect())
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

        /// Convert a connected test QP into the real Phase 3 connection path.
        pub fn install_connection(
            &self,
            qp: TestEngineQp,
            cm: AsyncCmId,
            config: RdmaConnectionConfig,
        ) -> Result<RdmaConnection> {
            let shared = self.ensure_active()?;
            if !qp.qp.uses_resources(&self.resources.pd, &self.resources.cq) {
                return Err(Error::InvalidConfig(
                    "test QP was not created from the leased engine PD/shared CQ".into(),
                ));
            }
            let local_addr = cm.cm_id().local_addr();
            let peer_addr = cm.cm_id().peer_addr();
            install_connection(
                &shared,
                Arc::new(VerbsConnectionResources::new(qp.qp, cm)),
                config,
                local_addr,
                peer_addr,
            )
        }

        /// Explicitly transition an installed Phase 3 connection to QP ERR.
        pub fn transition_connection_to_error(&self, connection: &RdmaConnection) -> Result<()> {
            let shared = self.ensure_not_terminal()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Err(Error::InvalidConfig(
                    "connection belongs to another engine".into(),
                ));
            }
            connection.transition_to_error_for_test()
        }

        /// Request an RDMA-CM disconnect for an outbound engine connection.
        pub fn disconnect_connection(&self, connection: &RdmaConnection) -> Result<()> {
            let shared = self.ensure_active()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Err(Error::InvalidConfig(
                    "connection belongs to another engine".into(),
                ));
            }
            connection.state.disconnect_for_test()
        }

        /// Make the next result-aware destruction of this connection's QP fail.
        pub fn fail_next_connection_qp_destroy(&self, connection: &RdmaConnection) -> Result<()> {
            let shared = self.ensure_active()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Err(Error::InvalidConfig(
                    "connection belongs to another engine".into(),
                ));
            }
            connection.state.poster.fail_next_qp_destroy()
        }

        /// Fail the next newly created connection installation and its QP rollback.
        ///
        /// A real MR is retained by the failed connection state so tests can
        /// prove rollback ownership is not released before the failed QP
        /// destruction boundary.
        pub fn fail_next_setup_rollback_qp_destroy(&self, error: Error) -> Result<()> {
            let shared = self.ensure_active()?;
            shared.test_driver.inject_setup_rollback_failure(error, || {
                self.resources.pd.reg_mr(64, AccessIntent::LocalOnly)
            })
        }

        /// Terminate the real driver on its next poll with an exact test error.
        pub fn inject_driver_failure(&self, error: Error) -> Result<()> {
            let shared = self.ensure_active()?;
            shared.test_driver.inject_failure(error)?;
            shared.work_signal.publish(TERMINAL_WORK);
            Ok(())
        }

        /// Pause the next CQ arm-to-post-poll window until released.
        ///
        /// Only one controller may be active for an engine. Dropping it
        /// cancels an unobserved request or releases the paused arm.
        pub fn pause_next_cq_arm_window(&self) -> Result<TestCqArmWindowControl> {
            let shared = self.ensure_active()?;
            shared
                .test_driver
                .start_cq_arm_control(CqArmRacePoint::AfterArm)?;
            Ok(TestCqArmWindowControl {
                shared: Arc::downgrade(&shared),
                point: CqArmRacePoint::AfterArm,
                active: true,
            })
        }

        /// Pause after the initial empty CQ poll and before notification arm.
        pub fn pause_next_cq_pre_arm_window(&self) -> Result<TestCqArmWindowControl> {
            let shared = self.ensure_active()?;
            shared
                .test_driver
                .start_cq_arm_control(CqArmRacePoint::BeforeArm)?;
            Ok(TestCqArmWindowControl {
                shared: Arc::downgrade(&shared),
                point: CqArmRacePoint::BeforeArm,
                active: true,
            })
        }

        /// Pause the next accepted connect after its shutdown check and before
        /// its request becomes visible to the driver.
        pub fn pause_next_connect_before_enqueue(&self) -> Result<TestAdmissionBarrier> {
            self.pause_next_admission(AdmissionPausePoint::ConnectBeforeEnqueue)
        }

        /// Pause the next accepted operation after its shutdown check and
        /// before operation registration and provider posting.
        pub fn pause_next_operation_before_register(&self) -> Result<TestAdmissionBarrier> {
            self.pause_next_admission(AdmissionPausePoint::OperationBeforeRegister)
        }

        /// Hold the next real CQE routed to `connection` after it is polled.
        ///
        /// The held CQE remains an actual provider completion and can later be
        /// released back through the normal exact production router.
        pub fn suppress_next_connection_cqe(
            &self,
            connection: &RdmaConnection,
        ) -> Result<TestConnectionCqeSuppression> {
            self.suppress_next_connection_cqe_matching(connection, None, false)
        }

        /// Hold the next real CQE with the requested opcode for `connection`.
        pub fn suppress_next_connection_cqe_with_opcode(
            &self,
            connection: &RdmaConnection,
            opcode: WcOpcode,
        ) -> Result<TestConnectionCqeSuppression> {
            self.suppress_next_connection_cqe_matching(connection, Some(opcode), false)
        }

        /// Hold the next real flush CQE for `connection` after polling.
        ///
        /// If the provider omits the flush CQE, the controller remains
        /// unobserved while the production QP-destruction fallback proceeds.
        pub fn suppress_next_connection_flush_cqe(
            &self,
            connection: &RdmaConnection,
        ) -> Result<TestConnectionCqeSuppression> {
            self.suppress_next_connection_cqe_matching(connection, None, true)
        }

        fn suppress_next_connection_cqe_matching(
            &self,
            connection: &RdmaConnection,
            expected_opcode: Option<WcOpcode>,
            require_flush: bool,
        ) -> Result<TestConnectionCqeSuppression> {
            let shared = self.ensure_active()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Err(Error::InvalidConfig(
                    "connection belongs to another engine".into(),
                ));
            }
            shared.test_driver.start_connection_cqe_suppression(
                connection.state.token,
                connection.identity().qp_num(),
                expected_opcode,
                require_flush,
            )?;
            Ok(TestConnectionCqeSuppression {
                shared: Arc::downgrade(&shared),
                connection: connection.state.token,
                qp_num: connection.identity().qp_num(),
                active: true,
            })
        }

        /// Return the exact accepted WR IDs currently owned by `connection`.
        pub fn accepted_operation_wr_ids(&self, connection: &RdmaConnection) -> Result<Vec<u64>> {
            let shared = self.ensure_active()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Err(Error::InvalidConfig(
                    "connection belongs to another engine".into(),
                ));
            }
            Ok(connection
                .state
                .accepted_tokens()
                .into_iter()
                .map(OperationToken::encode)
                .collect())
        }

        /// Return the private registry slot and generation for a connection.
        pub fn connection_registry_identity(
            &self,
            connection: &RdmaConnection,
        ) -> Result<(u32, u32)> {
            let shared = self.ensure_active()?;
            if !Arc::ptr_eq(&shared, &connection.shared) {
                return Err(Error::InvalidConfig(
                    "connection belongs to another engine".into(),
                ));
            }
            let identity = connection.identity();
            Ok((identity.registry_slot(), identity.registration_generation()))
        }

        /// Decode a test-observed WR ID into its operation slot and generation.
        pub fn operation_registry_identity(&self, wr_id: u64) -> Result<(u32, u32)> {
            self.ensure_active()?;
            let token = OperationToken::decode(wr_id);
            Ok((token.slot, token.generation))
        }

        /// Snapshot minimal CM ownership and rejected-CQE observations.
        pub fn instrumentation(&self) -> Result<TestEngineInstrumentation> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            Ok(shared.test_driver.instrumentation(&shared))
        }

        /// Inject a synthetic CQE through the production exact router.
        pub fn inject_completion(&self, wr_id: u64, qp_num: u32, opcode: WcOpcode) -> Result<()> {
            let shared = self.ensure_active()?;
            let mut completion = WorkCompletion::default();
            completion.inner.wr_id = wr_id;
            completion.inner.qp_num = qp_num;
            completion.inner.status = rdma_io_sys::ibverbs::IBV_WC_SUCCESS;
            completion.inner.opcode = raw_wc_opcode(opcode)?;
            if let Some(token) = shared.enqueue_completion(completion)
                && let Lookup::Occupied(connection) = shared.connections.lookup(token)
            {
                shared.publish_completion(&connection);
            }
            Ok(())
        }

        fn pause_next_admission(&self, point: AdmissionPausePoint) -> Result<TestAdmissionBarrier> {
            let shared = self.ensure_active()?;
            shared.test_driver.start_admission_control(point)?;
            Ok(TestAdmissionBarrier {
                shared: Arc::downgrade(&shared),
                point,
                active: true,
            })
        }

        fn ensure_active(&self) -> Result<Arc<EngineShared>> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            if shared.outcome().is_some() || shared.shutdown_requested.load(Ordering::Acquire) {
                return Err(Error::DriverShutdown);
            }
            Ok(shared)
        }

        fn ensure_not_terminal(&self) -> Result<Arc<EngineShared>> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            if shared.outcome().is_some() {
                return Err(Error::DriverShutdown);
            }
            Ok(shared)
        }
    }

    impl TestContextIdentity {
        /// Compare with one independently verbs-opened context of the same device.
        pub fn matches_independently_opened(&self, device_name: &str) -> Result<bool> {
            let independent =
                crate::device::open_device_by_name(device_name).map_err(Error::from_v1)?;
            let matches = independent.as_raw() as usize == self.raw_context;
            drop(independent);
            Ok(matches)
        }
    }

    impl TestProviderLimits {
        pub fn max_qp(&self) -> usize {
            self.max_qp
        }

        pub fn max_qp_wr(&self) -> usize {
            self.max_qp_wr
        }

        pub fn max_sge(&self) -> usize {
            self.max_sge
        }

        pub fn max_cqe(&self) -> usize {
            self.max_cqe
        }

        pub fn max_qp_rd_atom(&self) -> usize {
            self.max_qp_rd_atom
        }

        pub fn max_qp_init_rd_atom(&self) -> usize {
            self.max_qp_init_rd_atom
        }
    }

    impl TestAdmissionBarrier {
        /// Wait until the accepted request is paused while holding admission.
        pub fn wait_until_paused(&self) -> Result<()> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            shared.test_driver.wait_for_admission(self.point, false)
        }

        /// Wait until shutdown has reached the admission write barrier.
        pub fn wait_until_shutdown_attempted(&self) -> Result<()> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            shared.test_driver.wait_for_admission(self.point, true)
        }

        /// Release the accepted request to publish or post before shutdown.
        pub fn release(mut self) -> Result<()> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            shared.test_driver.release_admission(self.point)?;
            self.active = false;
            Ok(())
        }
    }

    impl Drop for TestAdmissionBarrier {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            if let Some(shared) = self.shared.upgrade() {
                shared.test_driver.stop_admission_control(self.point);
            }
            self.active = false;
        }
    }

    impl TestCqArmWindowControl {
        /// Wait until the driver is paused in an arm-to-post-poll window.
        pub async fn wait_for_pause_after(&self, previous: u64) -> Result<u64> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            loop {
                let notified = shared.test_driver.cq_arm_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let current = shared.test_driver.paused_generation(self.point);
                if current > previous {
                    return Ok(current);
                }
                if shared.outcome().is_some() {
                    return Err(Error::DriverShutdown);
                }
                notified.await;
            }
        }

        /// Resume the exact arm generation after the test posts its CQE.
        pub fn release(mut self, generation: u64) -> Result<()> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            shared.test_driver.release_cq_arm(self.point, generation)?;
            shared.work_signal.publish(0);
            self.active = false;
            Ok(())
        }
    }

    impl Drop for TestCqArmWindowControl {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            if let Some(shared) = self.shared.upgrade() {
                shared.test_driver.stop_cq_arm_control(self.point);
                shared.work_signal.publish(0);
            }
            self.active = false;
        }
    }

    impl TestConnectionCqeSuppression {
        /// Wait until the engine has polled and held the matching real CQE.
        pub async fn wait_observed(&self) -> Result<()> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            loop {
                let notified = shared.test_driver.connection_cqe_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if shared
                    .test_driver
                    .connection_cqe_is_observed(self.connection, self.qp_num)
                {
                    return Ok(());
                }
                if shared.outcome().is_some() {
                    return Err(Error::DriverShutdown);
                }
                notified.await;
            }
        }

        /// Return the held real CQE as a typed completion.
        pub fn completion(&self) -> Result<Completion> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            shared
                .test_driver
                .connection_cqe(self.connection, self.qp_num)
        }

        /// Release the held real CQE through normal exact production routing.
        pub fn release(mut self) -> Result<()> {
            let shared = self.shared.upgrade().ok_or(Error::DriverShutdown)?;
            shared
                .test_driver
                .release_connection_cqe(self.connection, self.qp_num)?;
            shared.work_signal.publish(0);
            self.active = false;
            Ok(())
        }
    }

    impl Drop for TestConnectionCqeSuppression {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            if let Some(shared) = self.shared.upgrade() {
                shared
                    .test_driver
                    .abandon_connection_cqe(self.connection, self.qp_num);
            }
            self.active = false;
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
        pub fn qp_num(&self) -> u32 {
            self.route.identity.qp_num
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

        /// Retain a resource only until the exact operation CQE is consumed.
        pub fn retain_until_completion<T: Send + 'static>(&self, wr_id: u64, value: T) {
            self.route
                .operation_retained
                .lock()
                .expect("test route operation resources poisoned")
                .insert(wr_id, Box::new(value));
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

        pub fn completions(&self) -> Vec<Completion> {
            self.route
                .completions
                .lock()
                .expect("test route completions poisoned")
                .iter()
                .copied()
                .map(Completion::from_raw)
                .collect()
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
        pub fn remove(mut self) -> Result<Vec<Completion>> {
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
        #[cfg(test)]
        next_idle_qp: AtomicU32,
        cq_arms: AtomicU64,
        cq_arm_controller_active: AtomicBool,
        cq_pre_arm_controlled: AtomicBool,
        cq_pre_arm_paused: AtomicU64,
        cq_arm_controlled: AtomicBool,
        cq_arm_paused: AtomicU64,
        cq_arm_notify: Notify,
        admission_barrier: AdmissionBarrierState,
        connection_cqe_suppression: Mutex<Option<ConnectionCqeSuppressionState>>,
        released_connection_cqes: Mutex<VecDeque<WorkCompletion>>,
        connection_cqe_notify: Notify,
        injected_failure: Mutex<Option<Error>>,
        setup_rollback_failure: Mutex<Option<SetupRollbackFailure>>,
    }

    struct ConnectionCqeSuppressionState {
        connection: super::super::registry::ConnectionToken,
        qp_num: u32,
        expected_opcode: Option<WcOpcode>,
        require_flush: bool,
        completion: Option<WorkCompletion>,
        abandoned: bool,
    }

    pub(in crate::v2::engine) struct SetupRollbackFailure {
        pub(in crate::v2::engine) error: Error,
        pub(in crate::v2::engine) retained_mr: Mr,
    }

    impl TestDriverState {
        pub(in crate::v2::engine) fn new() -> Self {
            Self {
                routes: Mutex::new(HashMap::new()),
                next_slot: AtomicU32::new(0),
                #[cfg(test)]
                next_idle_qp: AtomicU32::new(0x7000_0000),
                cq_arms: AtomicU64::new(0),
                cq_arm_controller_active: AtomicBool::new(false),
                cq_pre_arm_controlled: AtomicBool::new(false),
                cq_pre_arm_paused: AtomicU64::new(0),
                cq_arm_controlled: AtomicBool::new(false),
                cq_arm_paused: AtomicU64::new(0),
                cq_arm_notify: Notify::new(),
                admission_barrier: AdmissionBarrierState {
                    control: Mutex::new(None),
                    changed: Condvar::new(),
                },
                connection_cqe_suppression: Mutex::new(None),
                released_connection_cqes: Mutex::new(VecDeque::new()),
                connection_cqe_notify: Notify::new(),
                injected_failure: Mutex::new(None),
                setup_rollback_failure: Mutex::new(None),
            }
        }

        #[cfg(test)]
        fn next_idle_qp(&self) -> Result<u32> {
            self.next_idle_qp
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(1)
                })
                .map_err(|_| Error::CapacityExhausted)
        }

        #[cfg(test)]
        pub(in crate::v2::engine) fn install_idle_connections(
            &self,
            shared: &Arc<EngineShared>,
            count: usize,
        ) -> Result<Vec<RdmaConnection>> {
            let mut connections = Vec::new();
            connections
                .try_reserve_exact(count)
                .map_err(|_| Error::CapacityExhausted)?;
            for _ in 0..count {
                let qp_num = self.next_idle_qp()?;
                connections.push(install_connection(
                    shared,
                    Arc::new(TestIdlePoster { qp_num }),
                    RdmaConnectionConfig::default(),
                    None,
                    None,
                )?);
            }
            Ok(connections)
        }

        fn instrumentation(&self, shared: &EngineShared) -> TestEngineInstrumentation {
            TestEngineInstrumentation {
                cm_pending_routes: shared.cm.pending_route_count(),
                cm_retained_owners: shared.cm.retained_owner_count(),
                cqes_rejected: shared.io_core.rejected_cqe_count(),
                cm_events_rejected: shared.rejected_cm_events.load(Ordering::Acquire),
            }
        }

        fn inject_failure(&self, error: Error) -> Result<()> {
            let mut pending = self
                .injected_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending.is_some() {
                return Err(Error::InvalidConfig(
                    "a driver failure is already pending".into(),
                ));
            }
            *pending = Some(error);
            Ok(())
        }

        pub(super) fn take_injected_failure(&self) -> Option<Error> {
            self.injected_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        }

        fn inject_setup_rollback_failure(
            &self,
            error: Error,
            register_mr: impl FnOnce() -> Result<Mr>,
        ) -> Result<()> {
            let mut pending = self
                .setup_rollback_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending.is_some() {
                return Err(Error::InvalidConfig(
                    "a setup rollback failure is already pending".into(),
                ));
            }
            let retained_mr = register_mr()?;
            *pending = Some(SetupRollbackFailure { error, retained_mr });
            Ok(())
        }

        pub(in crate::v2::engine) fn take_setup_rollback_failure(
            &self,
        ) -> Option<SetupRollbackFailure> {
            self.setup_rollback_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        }

        fn start_connection_cqe_suppression(
            &self,
            connection: super::super::registry::ConnectionToken,
            qp_num: u32,
            expected_opcode: Option<WcOpcode>,
            require_flush: bool,
        ) -> Result<()> {
            let mut control = self
                .connection_cqe_suppression
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if control.is_some() {
                return Err(Error::InvalidConfig(
                    "connection CQE suppression is already active".into(),
                ));
            }
            *control = Some(ConnectionCqeSuppressionState {
                connection,
                qp_num,
                expected_opcode,
                require_flush,
                completion: None,
                abandoned: false,
            });
            Ok(())
        }

        pub(super) fn suppress_connection_cqe(&self, completion: WorkCompletion) -> bool {
            let mut guard = self
                .connection_cqe_suppression
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(control) = guard.as_mut() else {
                return false;
            };
            if control.completion.is_some()
                || control.qp_num != completion.qp_num()
                || control
                    .expected_opcode
                    .is_some_and(|opcode| opcode != completion.opcode())
                || (control.require_flush && completion.status() != WcStatus::WrFlushErr)
                || control.abandoned
            {
                return false;
            }
            control.completion = Some(completion);
            drop(guard);
            self.connection_cqe_notify.notify_waiters();
            true
        }

        fn connection_cqe_is_observed(
            &self,
            connection: super::super::registry::ConnectionToken,
            qp_num: u32,
        ) -> bool {
            self.connection_cqe_suppression
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .is_some_and(|control| {
                    control.connection == connection
                        && control.qp_num == qp_num
                        && control.completion.is_some()
                })
        }

        fn connection_cqe(
            &self,
            connection: super::super::registry::ConnectionToken,
            qp_num: u32,
        ) -> Result<Completion> {
            let control = self
                .connection_cqe_suppression
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let active = control.as_ref().ok_or_else(|| {
                Error::InvalidConfig("connection CQE suppression is not active".into())
            })?;
            if active.connection != connection || active.qp_num != qp_num {
                return Err(Error::InvalidConfig(
                    "connection CQE suppression identity changed".into(),
                ));
            }
            let completion = active.completion.ok_or_else(|| {
                Error::InvalidConfig("connection CQE has not been observed".into())
            })?;
            Ok(Completion::from_raw(completion))
        }

        fn release_connection_cqe(
            &self,
            connection: super::super::registry::ConnectionToken,
            qp_num: u32,
        ) -> Result<()> {
            let mut control = self
                .connection_cqe_suppression
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(active) = control.as_ref() else {
                return Err(Error::InvalidConfig(
                    "connection CQE suppression is not active".into(),
                ));
            };
            if active.connection != connection || active.qp_num != qp_num {
                return Err(Error::InvalidConfig(
                    "connection CQE suppression identity changed".into(),
                ));
            }
            let completion = active.completion.ok_or_else(|| {
                Error::InvalidConfig("connection CQE has not been observed".into())
            })?;
            *control = None;
            drop(control);
            self.released_connection_cqes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push_back(completion);
            Ok(())
        }

        fn abandon_connection_cqe(
            &self,
            connection: super::super::registry::ConnectionToken,
            qp_num: u32,
        ) {
            if let Some(control) = self
                .connection_cqe_suppression
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_mut()
                && control.connection == connection
                && control.qp_num == qp_num
            {
                control.abandoned = true;
            }
        }

        pub(super) fn take_released_connection_cqe(&self) -> Option<WorkCompletion> {
            self.released_connection_cqes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front()
        }

        #[cfg(test)]
        pub(super) fn queue_released_connection_cqe(&self, completion: WorkCompletion) {
            self.released_connection_cqes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push_back(completion);
        }

        pub(in crate::v2::engine) fn pause_admission(&self, point: AdmissionPausePoint) {
            let mut control = self
                .admission_barrier
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(active) = control.as_mut() else {
                return;
            };
            if active.point != point || active.released {
                return;
            }
            active.paused = true;
            self.admission_barrier.changed.notify_all();
            while control
                .as_ref()
                .is_some_and(|active| active.point == point && !active.released)
            {
                control = self
                    .admission_barrier
                    .changed
                    .wait(control)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if control
                .as_ref()
                .is_some_and(|active| active.point == point && active.released)
            {
                *control = None;
                self.admission_barrier.changed.notify_all();
            }
        }

        pub(in crate::v2::engine) fn record_shutdown_attempt(&self) {
            let mut control = self
                .admission_barrier
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(active) = control.as_mut() else {
                return;
            };
            if active.paused && !active.released {
                active.shutdown_attempted = true;
                self.admission_barrier.changed.notify_all();
            }
        }

        fn start_admission_control(&self, point: AdmissionPausePoint) -> Result<()> {
            let mut control = self
                .admission_barrier
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if control.is_some() {
                return Err(Error::InvalidConfig(
                    "engine admission barrier is already active".into(),
                ));
            }
            *control = Some(AdmissionControl {
                point,
                paused: false,
                shutdown_attempted: false,
                released: false,
            });
            Ok(())
        }

        fn wait_for_admission(
            &self,
            point: AdmissionPausePoint,
            shutdown_attempted: bool,
        ) -> Result<()> {
            let mut control = self
                .admission_barrier
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            loop {
                match control.as_ref() {
                    Some(active)
                        if active.point == point
                            && active.paused
                            && (!shutdown_attempted || active.shutdown_attempted) =>
                    {
                        return Ok(());
                    }
                    Some(active) if active.point != point || active.released => {
                        return Err(Error::InvalidConfig(
                            "engine admission barrier changed before observation".into(),
                        ));
                    }
                    None => {
                        return Err(Error::InvalidConfig(
                            "engine admission barrier is not active".into(),
                        ));
                    }
                    Some(_) => {}
                }
                let (next, timeout) = self
                    .admission_barrier
                    .changed
                    .wait_timeout(control, Duration::from_secs(10))
                    .unwrap_or_else(|error| error.into_inner());
                control = next;
                if timeout.timed_out() {
                    return Err(Error::InvalidConfig(
                        "timed out waiting for engine admission barrier".into(),
                    ));
                }
            }
        }

        fn release_admission(&self, point: AdmissionPausePoint) -> Result<()> {
            let mut control = self
                .admission_barrier
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(active) = control.as_mut() else {
                return Err(Error::InvalidConfig(
                    "engine admission barrier is not active".into(),
                ));
            };
            if active.point != point || !active.paused {
                return Err(Error::InvalidConfig(
                    "engine admission barrier is not paused at the requested point".into(),
                ));
            }
            active.released = true;
            self.admission_barrier.changed.notify_all();
            Ok(())
        }

        fn stop_admission_control(&self, point: AdmissionPausePoint) {
            let mut control = self
                .admission_barrier
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(active) = control.as_mut()
                && active.point == point
            {
                active.released = true;
                self.admission_barrier.changed.notify_all();
                if !active.paused {
                    *control = None;
                }
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
                identity: RouteIdentity {
                    slot,
                    generation: 1,
                    qp_num,
                },
                qp,
                retained: Mutex::new(Vec::new()),
                operation_retained: Mutex::new(HashMap::new()),
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
            shared.work_signal.publish(COMPLETION_DISPATCH_WORK);
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

        pub(super) fn record_cq_arm(&self, generation: u64) -> bool {
            let previous = self.cq_arms.swap(generation, Ordering::AcqRel);
            debug_assert!(generation > previous, "CQ arm generations must increase");
            self.cq_arm_notify.notify_waiters();
            if !self.cq_arm_controlled.swap(false, Ordering::AcqRel) {
                return false;
            }
            if self
                .cq_arm_paused
                .compare_exchange(0, generation, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                debug_assert!(false, "a CQ arm was already paused");
                return false;
            }
            self.cq_arm_notify.notify_waiters();
            true
        }

        pub(super) fn record_cq_pre_arm(&self, generation: u64) -> bool {
            if !self.cq_pre_arm_controlled.swap(false, Ordering::AcqRel) {
                return false;
            }
            if self
                .cq_pre_arm_paused
                .compare_exchange(0, generation, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                debug_assert!(false, "a CQ pre-arm window was already paused");
                return false;
            }
            self.cq_arm_notify.notify_waiters();
            true
        }

        fn start_cq_arm_control(&self, point: CqArmRacePoint) -> Result<()> {
            self.cq_arm_controller_active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| {
                    Error::InvalidConfig("CQ arm-window control is already active".into())
                })?;
            match point {
                CqArmRacePoint::BeforeArm => {
                    self.cq_pre_arm_controlled.store(true, Ordering::Release);
                }
                CqArmRacePoint::AfterArm => {
                    self.cq_arm_controlled.store(true, Ordering::Release);
                }
            }
            Ok(())
        }

        fn paused_generation(&self, point: CqArmRacePoint) -> u64 {
            match point {
                CqArmRacePoint::BeforeArm => self.cq_pre_arm_paused.load(Ordering::Acquire),
                CqArmRacePoint::AfterArm => self.cq_arm_paused.load(Ordering::Acquire),
            }
        }

        fn release_cq_arm(&self, point: CqArmRacePoint, generation: u64) -> Result<()> {
            let paused = match point {
                CqArmRacePoint::BeforeArm => &self.cq_pre_arm_paused,
                CqArmRacePoint::AfterArm => &self.cq_arm_paused,
            };
            paused
                .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|observed| {
                    Error::InvalidConfig(format!(
                        "CQ arm generation {generation} is not paused (observed {observed})"
                    ))
                })?;
            self.cq_arm_controller_active
                .store(false, Ordering::Release);
            Ok(())
        }

        fn stop_cq_arm_control(&self, point: CqArmRacePoint) {
            match point {
                CqArmRacePoint::BeforeArm => {
                    self.cq_pre_arm_controlled.store(false, Ordering::Release);
                    self.cq_pre_arm_paused.store(0, Ordering::Release);
                }
                CqArmRacePoint::AfterArm => {
                    self.cq_arm_controlled.store(false, Ordering::Release);
                    self.cq_arm_paused.store(0, Ordering::Release);
                }
            }
            self.cq_arm_controller_active
                .store(false, Ordering::Release);
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
        identity: RouteIdentity,
        qp: Arc<Qp>,
        retained: Mutex<Vec<Box<dyn Any + Send>>>,
        operation_retained: Mutex<HashMap<u64, Box<dyn Any + Send>>>,
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
            let retained = self
                .operation_retained
                .lock()
                .expect("test route operation resources poisoned")
                .remove(&wr_id);
            drop(retained);
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
    TestAcceptedOperation, TestAdmissionBarrier, TestConnectionCqeSuppression, TestContextIdentity,
    TestCqArmWindowControl, TestCqeRejection, TestCqeSuppression, TestEngineInstrumentation,
    TestEngineQp, TestEngineResources, TestProviderLimits, TestRouteHandle,
    TestSharedResourceIdentity,
};

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{RawWaker, RawWakerVTable, Waker};
    use std::time::Duration;

    use super::*;
    use crate::v2::engine::connection::{WorkRequestPoster, install_connection};
    use crate::v2::engine::io_core::{
        completion_for_driver_test, install_accepted_operation_for_driver_test,
    };
    use crate::v2::engine::listener::ListenerState;
    use crate::v2::engine::{
        RdmaConnectionConfig, RdmaEngineLifecycle, RdmaEngineTerminalError, RdmaListener,
        test_engine_pair,
    };
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

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
        signal.publish(COMPLETION_DISPATCH_WORK);
        let counter = CountingWaker::new();
        assert_eq!(
            signal.register_and_recheck(&counter.waker(), observed),
            COMPLETION_DISPATCH_WORK
        );
        assert_eq!(counter.count(), 1);
    }

    #[test]
    fn concurrent_producers_coalesce_without_losing_work_classes() {
        let signal = Arc::new(WorkSignal::new());
        std::thread::scope(|scope| {
            let mut producers = Vec::new();
            for bit in [TERMINAL_WORK, RECLAMATION_WORK, COMPLETION_DISPATCH_WORK] {
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
            TERMINAL_WORK | RECLAMATION_WORK | COMPLETION_DISPATCH_WORK
        );
    }

    struct DrainInterleavingPoster {
        qp_num: u32,
        destroys: AtomicUsize,
    }

    impl WorkRequestPoster for DrainInterleavingPoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            unreachable!("interleaving test installs an accepted operation directly")
        }

        fn post_recv(&self, _: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            unreachable!("interleaving test installs an accepted operation directly")
        }

        fn to_error(&self) -> Result<()> {
            Ok(())
        }

        fn destroy_qp(&self) -> Result<bool> {
            self.destroys.fetch_add(1, Ordering::AcqRel);
            Ok(true)
        }

        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cq_reclamation_ready_interleaving_dispatches_queued_success_and_flush_exactly() {
        for mode in [CompletionMode::Readiness, CompletionMode::Polling] {
            for (opcode, status) in [
                (
                    rdma_io_sys::ibverbs::IBV_WC_SEND,
                    rdma_io_sys::ibverbs::IBV_WC_SUCCESS,
                ),
                (
                    rdma_io_sys::ibverbs::IBV_WC_RECV,
                    rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR,
                ),
            ] {
                let (engine, mut driver) = test_engine_pair(mode);
                let poster = Arc::new(DrainInterleavingPoster {
                    qp_num: 71,
                    destroys: AtomicUsize::new(0),
                });
                let connection = install_connection(
                    &engine.shared,
                    Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
                    RdmaConnectionConfig::default(),
                    None,
                    None,
                )
                .unwrap();
                let expected = if opcode == rdma_io_sys::ibverbs::IBV_WC_RECV {
                    crate::wc::WcOpcode::Recv
                } else {
                    crate::wc::WcOpcode::Send
                };
                let operation = install_accepted_operation_for_driver_test(
                    &engine.shared,
                    &connection.state,
                    expected,
                );
                connection.state.begin_close();
                connection.state.transition_to_error_once().unwrap();
                engine.shared.test_driver.queue_released_connection_cqe(
                    completion_for_driver_test(operation, poster.qp_num, opcode, status),
                );
                assert!(driver.scheduler.deadlines().push(
                    tokio::time::Instant::now(),
                    DeadlineKind::ConnectionDrain,
                    connection.state.token.encode(),
                ));
                driver.scheduler.mark_class_ready(WorkClass::Cq);
                driver.scheduler.mark_class_ready(WorkClass::Reclamation);
                let waker = Waker::noop();
                let mut cx = TaskContext::from_waker(waker);

                assert_eq!(driver.scheduler.next_class(), Some(WorkClass::Cq));
                assert!(driver.service_cq(&mut cx).unwrap());
                assert_eq!(driver.scheduler.next_class(), Some(WorkClass::Reclamation));
                assert!(driver.service_reclamation().unwrap());
                assert_eq!(
                    driver.scheduler.next_class(),
                    Some(WorkClass::CompletionDispatch)
                );
                assert!(driver.service_completion_dispatch());

                let diagnostics = engine.diagnostics();
                assert_eq!(diagnostics.accepted_operations, 0);
                assert_eq!(diagnostics.registered_operations, 0);
                assert_eq!(poster.destroys.load(Ordering::Acquire), 0);

                engine.shared.finish(MemoizedTerminalResult::success());
                drop(driver);
            }
        }
    }

    #[test]
    fn cm_event_arriving_during_clear_is_drained_after_reregister() {
        #[derive(Default)]
        struct FakeReadiness {
            ready: bool,
            event_available: bool,
            polls: usize,
            clears: usize,
        }

        let state = Rc::new(RefCell::new(FakeReadiness {
            ready: true,
            ..FakeReadiness::default()
        }));
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let result = poll_readiness_events(
            &mut cx,
            8,
            {
                let state = Rc::clone(&state);
                move |_| {
                    let mut state = state.borrow_mut();
                    state.polls += 1;
                    if state.ready {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Pending
                    }
                }
            },
            {
                let state = Rc::clone(&state);
                move |_| {
                    let mut state = state.borrow_mut();
                    state.clears += 1;
                    if state.clears == 1 {
                        // Exact regression: a new event edge appears after the
                        // empty read but while the stale readiness is cleared.
                        state.event_available = true;
                        state.ready = true;
                    } else {
                        state.ready = false;
                    }
                }
            },
            {
                let state = Rc::clone(&state);
                move || {
                    let mut state = state.borrow_mut();
                    if state.event_available {
                        state.event_available = false;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(1))));
        let state = state.borrow();
        assert_eq!(
            state.polls, 3,
            "clear must be followed by a readiness re-poll"
        );
        assert_eq!(state.clears, 2);
        assert!(!state.event_available);
    }

    #[test]
    fn driver_poll_outside_tokio_returns_contextual_error_without_panicking() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut driver).poll(&mut cx),
            Poll::Ready(Err(Error::InvalidConfig(_)))
        ));
        assert_eq!(counter.count(), 0);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn driver_poll_without_tokio_time_returns_contextual_error_without_panicking() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let result = runtime
            .block_on(async { std::future::poll_fn(|cx| Pin::new(&mut driver).poll(cx)).await });
        assert!(matches!(result, Err(Error::InvalidConfig(_))));
    }

    #[cfg(not(panic = "unwind"))]
    #[test]
    fn abort_build_polling_driver_progresses_without_a_time_probe() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);

        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert_eq!(counter.count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_timer_wakes_driver_and_processes_due_work() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
        assert!(driver.scheduler.deadlines().push(
            tokio::time::Instant::now() + Duration::from_secs(5),
            DeadlineKind::Reclamation,
            7,
        ));
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);

        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert_eq!(counter.count(), 0, "an unexpired deadline stays idle");

        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(counter.count() > 0, "the Tokio timer must wake the driver");
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    }

    #[tokio::test]
    async fn readiness_idle_poll_does_not_self_wake_or_scan() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert_eq!(counter.count(), 0);
        assert_eq!(driver.scheduler.completion_connection_count(), 0);
    }

    #[tokio::test]
    async fn idle_connections_publish_no_completion_dispatch_work() {
        for count in [1, 1_024] {
            let mut config = super::super::config::EngineConfig::new("test0".into());
            config.completion_mode = CompletionMode::Readiness;
            config.max_live_connections = count;
            let shared = Arc::new(EngineShared::new(config, None, None).unwrap());
            let connections = shared
                .test_driver
                .install_idle_connections(&shared, count)
                .unwrap();
            let mut driver = super::super::RdmaEngineDriver::new(Arc::clone(&shared), None);
            let waker = futures_util::task::noop_waker();
            let mut cx = TaskContext::from_waker(&waker);

            assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
            assert_eq!(driver.scheduler.completion_connection_count(), 0);
            assert!(!shared.has_published_completions());

            drop(connections);
            drop(driver);
        }
    }

    #[tokio::test]
    async fn polling_empty_iteration_cooperatively_yields_once() {
        let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        {
            let mut cx = TaskContext::from_waker(&waker);
            assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        }
        tokio::task::yield_now().await;
        assert_eq!(counter.count(), 1);
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

    fn pending_destruction_listener(
        engine: &super::super::RdmaEngine,
    ) -> (RdmaListener, Arc<AtomicUsize>) {
        let state = ListenerState::test_only(1);
        let destroy_count = Arc::new(AtomicUsize::new(0));
        engine
            .shared
            .cm
            .defer_test_listener_destruction(Arc::clone(&state), Arc::clone(&destroy_count));
        (
            RdmaListener {
                shared: Arc::clone(&engine.shared),
                state,
            },
            destroy_count,
        )
    }

    fn assert_terminal_close(
        close: &mut Pin<Box<impl Future<Output = Result<()>>>>,
        cx: &mut TaskContext<'_>,
        expected: &RdmaEngineTerminalError,
    ) {
        let Poll::Ready(Err(error)) = close.as_mut().poll(cx) else {
            panic!("pending listener close was not terminalized");
        };
        assert_eq!(error.to_string(), expected.message);
    }

    #[test]
    fn driver_drop_wakes_listener_close_pending_cm_destruction() {
        let (engine, driver) = test_engine_pair(CompletionMode::Polling);
        let (listener, destroy_count) = pending_destruction_listener(&engine);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);
        let mut close = Box::pin(listener.close());

        assert!(close.as_mut().poll(&mut cx).is_pending());
        drop(driver);

        let terminal = engine
            .diagnostics()
            .terminal_error
            .expect("driver drop must publish a terminal error");
        assert_eq!(terminal.class, "EngineWedged");
        assert_terminal_close(&mut close, &mut cx, &terminal);
        assert_eq!(counter.count(), 1);
        assert_eq!(destroy_count.load(Ordering::Acquire), 0);
        assert_eq!(engine.shared.cm.retained_owner_count(), 1);
    }

    #[test]
    fn driver_error_wakes_listener_close_once_and_preserves_pending_destruction() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let (listener, destroy_count) = pending_destruction_listener(&engine);
        let counter = CountingWaker::new();
        let waker = counter.waker();
        let mut cx = TaskContext::from_waker(&waker);
        let mut close = Box::pin(listener.close());

        assert!(close.as_mut().poll(&mut cx).is_pending());
        let Poll::Ready(Err(driver_error)) = driver.fail(Error::InvalidConfig(
            "injected driver progress failure".into(),
        )) else {
            panic!("injected driver failure did not terminate the driver");
        };
        let terminal = engine
            .diagnostics()
            .terminal_error
            .expect("driver error must publish a terminal error");
        assert_eq!(driver_error.to_string(), terminal.message);
        assert_terminal_close(&mut close, &mut cx, &terminal);
        assert_eq!(counter.count(), 1);
        assert_eq!(destroy_count.load(Ordering::Acquire), 0);
        assert_eq!(engine.shared.cm.retained_owner_count(), 1);

        drop(driver);
        assert_eq!(counter.count(), 1, "driver drop must not finish twice");
        assert_eq!(
            engine
                .diagnostics()
                .terminal_error
                .expect("terminal error remains available"),
            terminal
        );
    }
}
