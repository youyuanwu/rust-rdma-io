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
#[cfg(test)]
use super::lifecycle::MemoizedTerminalResult;
use super::reactor::EngineReactor;
use super::resources::EngineReactorResources;
use super::session::SessionContext;
use super::{EngineFrontendRoot, RdmaEngineDriver};
use crate::v2::error::{Error, Result};
use crate::v2::runtime::preflight_driver_runtime;

/// A producer has made at least one reactor source potentially ready.
///
/// Source-specific owner bits are intentionally gone: each external poll
/// takes one finite ready-at-entry pass over the reactor source set.
pub(super) const REACTOR_WORK: usize = 1;

#[cfg(test)]
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

    pub(super) fn notify_reactor(&self) {
        self.pending
            .fetch_or(REACTOR_WORK, std::sync::atomic::Ordering::Release);
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
    pub(super) fn new(
        shared: Arc<EngineFrontendRoot>,
        session: SessionContext,
        resources: Option<EngineReactorResources>,
    ) -> Self {
        let reactor = EngineReactor::new(&shared, session, resources);
        Self {
            shared,
            reactor,
            deadline_sleep: None,
            deadline_at: None,
            runtime_checked: false,
        }
    }

    fn fail(&mut self, error: Error, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        self.reactor.begin_driver_failure(&self.shared, error);
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn release_resources(&mut self) {
        self.reactor.release_resources();
    }

    fn poll_deadline_timer(&mut self, cx: &mut TaskContext<'_>) -> bool {
        let next = self.reactor.next_deadline();
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
        true
    }
}

impl Future for RdmaEngineDriver {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if let Some(outcome) = self.reactor.lifecycle.outcome() {
            if !self.reactor.requires_complete_quarantine() {
                self.release_resources();
            }
            return Poll::Ready(outcome.into_result());
        }

        if let Some(error) = self.shared.take_driver_failure() {
            let shared = Arc::clone(&self.shared);
            self.reactor.begin_driver_failure(&shared, error);
        }
        let terminalizing_failure = self.reactor.lifecycle.is_terminalizing_failure();
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
        let shared = Arc::clone(&self.shared);
        self.reactor.transition_running(&shared);
        if !terminalizing_failure {
            self.poll_deadline_timer(cx);
        }

        let observed_epoch = self.shared.work_signal.epoch();
        self.shared.work_signal.take();
        let mode = self.shared.config.completion_mode;
        let shared = Arc::clone(&self.shared);
        let turn = match self.reactor.turn(&shared, mode, cx) {
            Ok(turn) => turn,
            Err(failure) => {
                let shared = Arc::clone(&self.shared);
                self.reactor
                    .begin_driver_failure(&shared, failure.error.clone());
                failure.actions.publish();
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        let requires_repoll = turn.requires_repoll;
        turn.actions.publish();

        if let Some(outcome) = self.reactor.lifecycle.outcome() {
            // A failed terminal result may still own an uncertain provider
            // bundle. Keep the canonical resource root in this driver until
            // Drop atomically moves the complete reactor into quarantine.
            if !self.reactor.requires_complete_quarantine() {
                self.release_resources();
            }
            return Poll::Ready(outcome.into_result());
        }

        if self.reactor.lifecycle.is_terminalizing_failure() {
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
                if published != 0 || requires_repoll {
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
        let actions = self.reactor.terminate_on_driver_drop(&self.shared);
        actions.publish();
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
