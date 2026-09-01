//! Async CQ poller with smoltcp-style waker registration.
//!
//! Provides [`CqPoller`], an async-native CQ polling primitive that directly
//! polls the RDMA completion queue and integrates with Rust async runtimes
//! through waker registration. This is the **polling-based** CQ integration
//! model, complementing the **fd/readiness-based** model in [`super::completion`].
//!
//! # Pattern
//!
//! Follows the smoltcp register–check–recheck pattern (identical to the v1
//! [`ConnSlot::poll_inbox`](crate::conn_slot::ConnSlot::poll_inbox) pattern):
//!
//! 1. Poll the CQ — if completions found, return `Ready`
//! 2. Register the task's waker
//! 3. Re-poll the CQ — catches the race where a completion arrived between
//!    step 1 and step 2
//! 4. If still empty, return `Pending`
//!
//! The waker is triggered by external code calling [`CqPoller::wake()`] —
//! typically a driver task, a timer, or application logic that knows
//! completions may be available (e.g., after posting work requests).
//!
//! # When to Use
//!
//! Use `CqPoller` when you want to directly poll the RDMA CQ without a
//! completion channel (no fd, no kernel notification overhead). This is
//! the async-native equivalent of the v1's direct CQ polling path, suitable
//! for:
//! - Busy-poll patterns driven by an external wake source
//! - Timer-driven periodic CQ checks
//! - Integration with custom polling drivers
//!
//! For fd/readiness-based CQ notification (where the kernel notifies the
//! async runtime when completions arrive), use [`Completions`](super::Completions)
//! instead.

use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::Completion;
use super::cq::Cq;
use super::error::Result;

/// Async CQ poller with waker registration for Rust async runtimes.
///
/// Wraps a [`Cq`] and provides `Poll`-based CQ polling following the
/// smoltcp/v1-ConnSlot pattern: poll → register waker → re-poll → Pending.
///
/// # Waker Contract
///
/// When `poll_completions` returns `Pending`, the poller stores the task's
/// waker. External code must call [`wake()`](CqPoller::wake) to trigger
/// a re-poll when completions may be available.
///
/// # Example
///
/// ```no_run
/// # use rdma_io::v2::*;
/// # use std::future::poll_fn;
/// # async fn example(ctx: &Context) -> Result<()> {
/// let cq = CqBuilder::new(ctx, 32).build()?; // poll-only, no channel
/// let poller = CqPoller::new(cq);
///
/// // In an async task, poll for completions:
/// let mut buf = [Completion::default(); 16];
/// let n = poll_fn(|cx| poller.poll_completions(cx, &mut buf)).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Use case
///
/// Integrate a poll-only CQ with an externally supplied wake source.
///
/// # Ownership and progress
///
/// The poller owns the CQ; callers own wake scheduling and task polling.
///
/// # Safety and limits
///
/// Only typed [`Completion`] buffers are exposed, and one logical consumer
/// must own CQ draining.
///
/// # Availability
///
/// Available with the `async` feature.
pub struct CqPoller {
    cq: Cq,
    waker: AtomicWaker,
}

impl CqPoller {
    /// Create a new async CQ poller.
    ///
    /// Works with any [`Cq`] (poll-only or channel-backed), but is designed
    /// primarily for poll-only CQs. For channel-backed CQs, prefer
    /// [`Completions`](super::Completions) which uses the more efficient
    /// fd-based arm-drain pattern.
    pub fn new(cq: Cq) -> Self {
        Self {
            cq,
            waker: AtomicWaker::new(),
        }
    }

    /// Poll the CQ for completions with async waker registration.
    ///
    /// Follows the register–check–recheck pattern:
    /// 1. Poll the RDMA CQ for pending completions
    /// 2. If found → `Ready(Ok(n))`
    /// 3. If empty → register the task waker, re-poll (race guard)
    /// 4. If still empty → `Pending`
    ///
    /// After returning `Pending`, the task is woken when external code
    /// calls [`wake()`](CqPoller::wake).
    pub fn poll_completions(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [Completion],
    ) -> Poll<Result<usize>> {
        // 1. Fast path: poll before touching the waker.
        let n = self.cq.poll(buf)?;
        if n > 0 {
            return Poll::Ready(Ok(n));
        }

        // 2. Empty: register waker, then re-poll to catch a completion
        //    that arrived between step 1 and the registration.
        self.waker.register(cx.waker());

        let n = self.cq.poll(buf)?;
        if n > 0 {
            return Poll::Ready(Ok(n));
        }

        // 3. Still empty — return Pending; external wake() will re-trigger.
        Poll::Pending
    }

    /// Wake the registered task waker.
    ///
    /// Call this when completions may be available on the CQ — the
    /// registered async task will be re-polled by its runtime.
    ///
    /// Typical wake sources:
    /// - After posting work requests (completion expected soon)
    /// - A timer/interval task for periodic checking
    /// - An external driver or busy-poll loop
    ///
    /// No-op if no waker is currently registered.
    pub fn wake(&self) {
        self.waker.wake();
    }

    /// Access the underlying CQ.
    pub fn cq(&self) -> &Cq {
        &self.cq
    }
}

// Safety: CqPoller is Send + Sync because:
// - Cq is Send + Sync (ibv_cq is thread-safe)
// - AtomicWaker is Send + Sync by design
unsafe impl Send for CqPoller {}
unsafe impl Sync for CqPoller {}

#[cfg(test)]
mod tests {
    use super::super::context::Context as V2Context;
    use super::super::cq::CqBuilder;
    use super::super::error::Error;
    use super::*;

    #[test]
    fn test_cq_poller_creation() {
        match V2Context::open_first() {
            Ok(ctx) => {
                let cq = CqBuilder::new(&ctx, 16).build().unwrap();
                let poller = CqPoller::new(cq);
                // Verify the poller was created successfully
                assert!(!poller.cq().has_channel());

                // wake() should be a no-op when no waker registered
                poller.wake();
            }
            Err(Error::NoDevices) => {}
            Err(e) => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn test_cq_poller_poll_empty_returns_pending() {
        match V2Context::open_first() {
            Ok(ctx) => {
                let cq = CqBuilder::new(&ctx, 16).build().unwrap();
                let poller = CqPoller::new(cq);
                let mut completions = [Completion::default(); 4];
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());

                assert!(matches!(
                    poller.poll_completions(&mut cx, &mut completions),
                    Poll::Pending
                ));
            }
            Err(Error::NoDevices) => {}
            Err(e) => panic!("unexpected: {e}"),
        }
    }
}
