//! Async Completion Queue poller.
//!
//! `AsyncCq` wraps a `CompletionQueue` + `CompletionChannel` + runtime `CqNotifier`
//! to provide async CQ polling without spin loops. Uses the standard drain-after-arm
//! pattern to avoid the race condition between arming and blocking.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};

use rdma_io_sys::ibverbs::*;

use crate::Result;
use crate::comp_channel::CompletionChannel;
use crate::cq::CompletionQueue;
use crate::wc::WorkCompletion;

/// Ack CQ events every this many events to amortize mutex cost.
const ACK_BATCH_SIZE: u32 = 16;

/// Trait abstracting over async runtimes for CQ fd readiness.
///
/// Each runtime provides an implementation that registers the
/// comp_channel fd with its reactor and awaits readiness.
///
/// # Edge-triggered semantics
///
/// Implementations use edge-triggered epoll (EPOLLET). Callers must
/// drain ALL events from the fd after readiness is signaled to avoid
/// lost notifications.
pub trait CqNotifier: Send + Sync {
    /// Wait until the comp_channel fd is readable.
    ///
    /// Returns when the fd has data (a CQ event notification).
    /// The caller must then drain all events and re-arm.
    fn readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>>;

    /// Poll-based readiness check for the comp_channel fd.
    ///
    /// Returns `Ready(Ok(()))` when the fd is (or was recently) readable.
    /// The caller must drain all events from the comp_channel to avoid
    /// lost wakeups with edge-triggered epoll.
    fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

/// State for poll-based CQ completion drain.
///
/// Tracks position in the drain-after-arm loop for `poll_completions()`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CqPollState {
    /// Start a fresh drain-after-arm cycle.
    #[default]
    Idle,
    /// CQ was armed and polled empty; waiting for fd readiness.
    WaitingFd,
}

/// Async completion queue poller.
///
/// Uses the drain-after-arm pattern:
/// 1. `req_notify_cq()` — arm CQ notification
/// 2. `poll_cq()` — drain any completions (catches arm/await race)
/// 3. If completions found → return them
/// 4. If empty → `notifier.readable().await` (sleep until fd fires)
/// 5. `get_cq_event()` + periodic `ack_cq_events()` — consume notification
/// 6. Loop back to 1
pub struct AsyncCq {
    cq: Arc<CompletionQueue>,
    channel: CompletionChannel,
    notifier: Box<dyn CqNotifier>,
    unacked_events: AtomicU32,
}

// Safety: All interior state is Send. The AtomicU32 is inherently Sync,
// and we only access CQ/channel from &self (no mutable aliasing).
unsafe impl Send for AsyncCq {}

impl AsyncCq {
    /// Create a new async CQ poller.
    ///
    /// The `cq` must have been created with `CompletionQueue::with_comp_channel`
    /// using the same `channel`.
    pub fn new(
        cq: Arc<CompletionQueue>,
        channel: CompletionChannel,
        notifier: Box<dyn CqNotifier>,
    ) -> Self {
        Self {
            cq,
            channel,
            notifier,
            unacked_events: AtomicU32::new(0),
        }
    }

    /// Poll for up to `wc_buf.len()` completions asynchronously.
    ///
    /// Returns when at least one completion is available.
    pub async fn poll(&self, wc_buf: &mut [WorkCompletion]) -> Result<usize> {
        loop {
            // 1. Arm notification
            self.cq.req_notify(false)?;

            // 2. Drain any completions (catches race between arm and await)
            let n = self.cq.poll(wc_buf)?;
            if n > 0 {
                return Ok(n);
            }

            // 3. No completions — wait for fd readiness
            self.notifier
                .readable()
                .await
                .map_err(crate::Error::Verbs)?;

            // 4. Drain all CQ events (EPOLLET safety)
            self.drain_channel_events()?;

            // 5. Loop back — poll will find completions now
        }
    }

    /// Wait for a specific WR ID completion.
    ///
    /// Any non-matching completions encountered are discarded.
    /// For production use with multiple in-flight WRs, use `poll()` directly
    /// and implement your own dispatch.
    pub async fn poll_wr_id(&self, expected: u64) -> Result<WorkCompletion> {
        let mut wc = [WorkCompletion::default(); 4];
        loop {
            let n = self.poll(&mut wc).await?;
            for item in &wc[..n] {
                if item.wr_id() == expected {
                    return Ok(*item);
                }
            }
        }
    }

    /// Access the underlying CQ.
    pub fn cq(&self) -> &Arc<CompletionQueue> {
        &self.cq
    }

    /// Poll-based completion drain using the drain-after-arm pattern.
    ///
    /// External `state` tracks where we are in the arm → drain → wait loop.
    /// Used by `AsyncRead`/`AsyncWrite` trait impls that need `Poll`-based APIs.
    ///
    /// # Edge-triggered safety
    ///
    /// After `poll_readable` returns Ready (which clears tokio's readiness
    /// flag), we drain ALL events from the comp_channel. This ensures
    /// EPOLLET correctly fires for the next new event.
    pub fn poll_completions(
        &self,
        cx: &mut Context<'_>,
        state: &mut CqPollState,
        wc_buf: &mut [WorkCompletion],
    ) -> Poll<Result<usize>> {
        loop {
            if *state == CqPollState::WaitingFd {
                match self.notifier.poll_readable(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(crate::Error::Verbs(e))),
                    Poll::Ready(Ok(())) => {
                        // Drain ALL comp_channel events to stay safe with EPOLLET.
                        // poll_readable cleared tokio's readiness flag, so we must
                        // empty the fd completely — any leftover event won't trigger
                        // a new edge and would be lost.
                        self.drain_channel_events()?;
                        *state = CqPollState::Idle;
                    }
                }
            }

            // Arm + drain
            self.cq.req_notify(false)?;
            let n = self.cq.poll(wc_buf)?;
            if n > 0 {
                return Poll::Ready(Ok(n));
            }

            // No completions — wait for fd
            match self.notifier.poll_readable(cx) {
                Poll::Pending => {
                    *state = CqPollState::WaitingFd;
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(crate::Error::Verbs(e))),
                Poll::Ready(Ok(())) => {
                    self.drain_channel_events()?;
                    // Loop back to arm+drain
                }
            }
        }
    }

    /// Drain all pending events from the comp_channel.
    ///
    /// Required after `poll_readable` (which clears tokio's edge-triggered
    /// readiness flag) to ensure the fd is truly empty. Any leftover event
    /// would not trigger a new EPOLLET notification.
    fn drain_channel_events(&self) -> Result<()> {
        loop {
            match self.channel.get_cq_event() {
                Ok(_) => self.ack_event(),
                Err(crate::Error::Verbs(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Ack one event, batching to amortize mutex cost.
    fn ack_event(&self) {
        let prev = self.unacked_events.fetch_add(1, Ordering::Relaxed);
        if prev + 1 >= ACK_BATCH_SIZE {
            let unacked = self.unacked_events.swap(0, Ordering::Relaxed);
            if unacked > 0 {
                unsafe {
                    ibv_ack_cq_events(self.cq.as_raw(), unacked);
                }
            }
        }
    }
}

impl Drop for AsyncCq {
    fn drop(&mut self) {
        // Drain any pending comp_channel events (from arm-before-poll races)
        // before acking and destroying the CQ.
        while self.channel.get_cq_event().is_ok() {
            self.unacked_events
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // Ack all remaining events before CQ destruction
        let unacked = self.unacked_events.load(Ordering::Relaxed);
        if unacked > 0 {
            unsafe {
                ibv_ack_cq_events(self.cq.as_raw(), unacked);
            }
        }
    }
}
