//! Sole engine-progress future and thin bounded scheduler.
//!
//! Software producers publish queue state before setting a pending class bit
//! and incrementing an epoch, then wake the registered `AtomicWaker`. Before
//! returning `Pending`, the driver registers its waker and rechecks both the
//! pending bits and epoch. Therefore a publish before registration is found by
//! the recheck, while a publish after registration performs the wake.
//!
//! Each poll fairly visits ready-at-entry I/O and session owners at most once,
//! then composes terminal eligibility as a bounded epilogue. The owners hide
//! CQ/CM readiness, completion routing, deadline kinds, teardown, and
//! terminalization details behind bounded progress reports. Because readiness
//! wakes do not identify their source, every poll probes both owners once;
//! idle owners register and recheck readiness without creating a self-wake
//! loop. Polling mode yields cooperatively after the bounded pass.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use super::config::CompletionMode;
use super::io_core::{IoProgress, IoSessionBridge};
#[cfg(test)]
use super::lifecycle::MemoizedTerminalResult;
use super::progress::OwnerClass;
use super::reactor::DriverTermination;
use super::resources::EngineResources;
use super::scheduler::OwnerScheduler;
use super::session::SessionProgress;
use super::{EngineShared, RdmaEngineDriver};
use crate::v2::error::{Error, Result};
use crate::v2::runtime::preflight_driver_runtime;

pub(super) const IO_WORK: usize = 1 << 0;
pub(super) const SESSION_WORK: usize = 1 << 1;
pub(super) const COMMAND_WORK: usize = 1 << 2;

fn earliest_deadline(
    io: Option<tokio::time::Instant>,
    session: Option<tokio::time::Instant>,
) -> Option<tokio::time::Instant> {
    match (io, session) {
        (Some(io), Some(session)) => Some(io.min(session)),
        (Some(io), None) => Some(io),
        (None, Some(session)) => Some(session),
        (None, None) => None,
    }
}

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

    #[cfg(test)]
    pub(in crate::v2::engine) fn register_waker_for_test(&self, waker: &std::task::Waker) {
        self.waker.register(waker);
    }
}

impl RdmaEngineDriver {
    pub(super) fn new(shared: Arc<EngineShared>, resources: Option<EngineResources>) -> Self {
        let (io_resources, session_resources) = match resources {
            Some(mut resources) => {
                let io = resources.take_io_progress_resources();
                (Some(io), Some(resources.into_session_progress()))
            }
            None => (None, None),
        };
        let bridge: Arc<dyn IoSessionBridge> = shared.session.clone();
        let io_progress = IoProgress::new(
            Arc::clone(&shared.io_core),
            bridge,
            io_resources,
            shared.config.cq_completion_budget,
            shared.config.completion_dispatch_budget,
            shared.config.io_reclamation_budget,
            #[cfg(any(test, feature = "test-hooks"))]
            Arc::clone(&shared.test_driver),
        );
        let session_progress = SessionProgress::new(
            Arc::clone(&shared.session),
            session_resources,
            shared.config.cm_event_budget,
            shared.config.session_reclamation_budget,
        );
        Self {
            shared,
            io_progress,
            session_progress,
            scheduler: OwnerScheduler::new(),
            deadline_sleep: None,
            deadline_at: None,
            runtime_checked: false,
        }
    }

    fn mark_published_work(&mut self, published: usize) {
        if published & IO_WORK != 0 {
            self.scheduler.mark_ready(OwnerClass::Io);
        }
        if published & SESSION_WORK != 0 {
            self.scheduler.mark_ready(OwnerClass::Session);
        }
    }

    fn service_commands(&mut self, published: usize) {
        if published & COMMAND_WORK == 0 {
            return;
        }
        let report = self.shared.commands.service_turn(&self.shared);
        if report.session_work {
            self.scheduler.mark_ready(OwnerClass::Session);
        }
        if report.has_more {
            self.shared.work_signal.publish(COMMAND_WORK);
        }
    }

    fn probe_owners(&mut self) {
        self.scheduler.mark_ready(OwnerClass::Io);
        self.scheduler.mark_ready(OwnerClass::Session);
    }

    fn fail(&mut self, error: Error, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        self.shared.begin_driver_failure(error);
        self.scheduler.mark_ready(OwnerClass::Io);
        self.scheduler.mark_ready(OwnerClass::Session);
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn release_resources(&mut self) {
        self.io_progress.release_resources();
        self.session_progress.release_resources();
    }

    fn service_io(&mut self, cx: &mut TaskContext<'_>) -> Result<bool> {
        let report = self
            .io_progress
            .turn(self.shared.config.completion_mode, cx)?;
        if report.requires_repoll() {
            self.scheduler.mark_ready(OwnerClass::Io);
        }
        Ok(report.units_consumed > 0)
    }

    fn service_session(&mut self, cx: &mut TaskContext<'_>) -> Result<bool> {
        let report = self
            .session_progress
            .turn(self.shared.config.completion_mode, cx)?;
        if report.requires_repoll() {
            self.scheduler.mark_ready(OwnerClass::Session);
        }
        Ok(report.units_consumed > 0)
    }

    fn poll_deadline_timer(&mut self, cx: &mut TaskContext<'_>) -> bool {
        let next = earliest_deadline(
            self.io_progress.next_deadline(),
            self.session_progress.next_deadline(),
        );
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
        self.probe_owners();
        true
    }

    fn poll_once(&mut self, cx: &mut TaskContext<'_>) -> Result<()> {
        let owner_budget = self.scheduler.begin_pass();
        for _ in 0..owner_budget {
            let Some(owner) = self.scheduler.next() else {
                break;
            };
            match owner {
                OwnerClass::Io => {
                    self.service_io(cx)?;
                }
                OwnerClass::Session => {
                    self.service_session(cx)?;
                }
            }
        }
        self.shared
            .progress_driver_terminal(&self.io_progress, &self.session_progress);
        Ok(())
    }
}

impl Future for RdmaEngineDriver {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if let Some(outcome) = self.shared.outcome() {
            self.release_resources();
            return Poll::Ready(outcome.into_result());
        }

        let terminalizing_failure = self.shared.pending_terminal_outcome().is_some();
        if !terminalizing_failure && !self.runtime_checked {
            if let Err(error) = preflight_driver_runtime("RdmaEngineDriver") {
                return self.fail(error, cx);
            }
            self.runtime_checked = true;
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if !terminalizing_failure
            && let Some(error) = self.shared.test_driver.take_injected_failure()
        {
            return self.fail(error, cx);
        }
        self.shared.transition_running();
        if !terminalizing_failure {
            self.poll_deadline_timer(cx);
        }

        let observed_epoch = self.shared.work_signal.epoch();
        let published = self.shared.work_signal.take();
        self.service_commands(published);
        self.mark_published_work(published);
        if let Err(error) = self.poll_once(cx) {
            return self.fail(error, cx);
        }

        if let Some(outcome) = self.shared.outcome() {
            self.release_resources();
            return Poll::Ready(outcome.into_result());
        }

        if self.shared.pending_terminal_outcome().is_some() {
            cx.waker().wake_by_ref();
            return Poll::Pending;
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
                if published & COMMAND_WORK != 0 {
                    // The command source is consumed only at the beginning of
                    // an external poll. Preserve a command bit observed during
                    // the final register/recheck for that next poll.
                    self.shared.work_signal.publish(COMMAND_WORK);
                }
                self.mark_published_work(published);
                if self.scheduler.ready_count() > 0 {
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
        DriverTermination::terminate(&self.shared);
        self.release_resources();
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub(super) mod test_api;

#[cfg(any(test, feature = "test-hooks"))]
pub use test_api::{
    TestAcceptedOperation, TestAdmissionBarrier, TestConnectionCqeSuppression, TestContextIdentity,
    TestCqArmWindowControl, TestCqeRejection, TestCqeSuppression, TestEngineInstrumentation,
    TestEngineQp, TestEngineResources, TestProviderLimits, TestRouteHandle,
    TestSharedResourceIdentity,
};

#[cfg(test)]
mod tests;
