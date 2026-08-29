//! Async CQ completion integration for Rust async runtimes.
//!
//! Provides [`Completions`], a cancellation-safe async wrapper around
//! a channel-backed completion queue. Generic over [`CqNotifier`] to
//! support different Rust async runtimes (Tokio, smol, async-io, etc.).
//!
//! This module provides the RDMA/CQ integration primitive — it does not
//! implement event-loop infrastructure, executors, or reactors.
//!
//! # Cancellation Safety
//!
//! [`Completions::next()`] is cancellation-safe: dropping the future
//! between await points does not lose completions or leave the CQ in
//! an inconsistent state. All intermediate state is kept inside the
//! [`Completions`] struct.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};

use crate::async_cq::CqNotifier;
use crate::wc::WorkCompletion;

use super::cq::Cq;
use super::error::{Error, Result};

/// Ack CQ events in batches to amortize kernel call cost.
const ACK_BATCH_SIZE: u32 = 16;

/// State for the drain-after-arm loop.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PollState {
    /// Start a fresh drain-after-arm cycle.
    #[default]
    Idle,
    /// CQ was armed and polled empty; waiting for fd readiness.
    WaitingFd,
}

/// Async CQ completion poller for a channel-backed [`Cq`].
///
/// Wraps a [`Cq`] (with completion channel) and a [`CqNotifier`]
/// to provide async CQ draining using the drain-after-arm pattern,
/// suitable for integration with Rust async runtimes.
///
/// # Type Parameter
///
/// `N` is the notifier implementation for a specific Rust async runtime —
/// typically [`TokioCqNotifier`] for Tokio. Any [`CqNotifier`] implementor
/// works (e.g., for smol, async-io, etc.).
///
/// [`TokioCqNotifier`]: crate::tokio_notifier::TokioCqNotifier
///
/// # Example
///
/// ```no_run
/// # use rdma_io::v2::*;
/// # async fn example() -> Result<()> {
/// let ctx = Context::open_first()?;
/// let cq = CqBuilder::new(&ctx, 32).with_channel().build()?;
/// let mut completions = cq.completions_tokio()?;
/// let mut buf = [rdma_io::wc::WorkCompletion::default(); 16];
/// let n = completions.next(&mut buf).await?;
/// println!("Got {n} completions");
/// # Ok(())
/// # }
/// ```
pub struct Completions<N: CqNotifier> {
    cq: Cq,
    notifier: N,
    state: PollState,
    unacked_events: AtomicU32,
}

impl<N: CqNotifier> Completions<N> {
    /// Create a new async completions poller.
    ///
    /// The `cq` must have been created with a completion channel
    /// (via [`CqBuilder::with_channel()`](super::CqBuilder::with_channel)).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if the CQ has no completion channel
    pub fn new(cq: Cq, notifier: N) -> Result<Self> {
        if !cq.has_channel() {
            return Err(Error::InvalidConfig(
                "Completions requires a channel-backed CQ (use CqBuilder::with_channel())".into(),
            ));
        }
        Ok(Self {
            cq,
            notifier,
            state: PollState::Idle,
            unacked_events: AtomicU32::new(0),
        })
    }

    /// Await the next batch of completions.
    ///
    /// Fills `buf` with up to `buf.len()` completed operations and returns
    /// the count. Blocks (asynchronously) until at least one completion is
    /// available.
    ///
    /// # Cancellation Safety
    ///
    /// This method is cancellation-safe. If the future is dropped before
    /// completion, no completions are lost and the CQ remains in a
    /// consistent state.
    pub async fn next(&mut self, buf: &mut [WorkCompletion]) -> Result<usize> {
        loop {
            match self.state {
                PollState::Idle => {
                    // 1. Arm CQ notification
                    self.cq.inner().req_notify(false)?;

                    // 2. Drain any pending completions (catches arm-race)
                    let n = self.cq.poll(buf)?;
                    if n > 0 {
                        return Ok(n);
                    }

                    // 3. No completions — need to wait for fd
                    self.state = PollState::WaitingFd;
                }
                PollState::WaitingFd => {
                    // 4. Wait for fd readiness
                    self.notifier.readable().await.map_err(Error::Verbs)?;

                    // 5. Drain all channel events (EPOLLET safety)
                    self.drain_channel_events()?;

                    // 6. Back to idle for next arm-drain cycle
                    self.state = PollState::Idle;
                }
            }
        }
    }

    /// Poll-based completion interface for manual event-loop integration.
    ///
    /// Returns `Poll::Ready(Ok(n))` when completions are available,
    /// or `Poll::Pending` when waiting for fd readiness.
    pub fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [WorkCompletion],
    ) -> Poll<Result<usize>> {
        loop {
            match self.state {
                PollState::Idle => {
                    // Arm notification
                    match self.cq.inner().req_notify(false) {
                        Ok(()) => {}
                        Err(e) => return Poll::Ready(Err(e.into())),
                    }

                    // Drain pending
                    match self.cq.poll(buf) {
                        Ok(n) if n > 0 => return Poll::Ready(Ok(n)),
                        Ok(_) => {
                            self.state = PollState::WaitingFd;
                        }
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                }
                PollState::WaitingFd => {
                    // Check fd readiness
                    match self.notifier.poll_readable(cx) {
                        Poll::Ready(Ok(())) => {
                            // Drain channel events
                            match self.drain_channel_events() {
                                Ok(()) => {
                                    self.state = PollState::Idle;
                                    // Continue loop to re-arm and drain
                                }
                                Err(e) => return Poll::Ready(Err(e)),
                            }
                        }
                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(Error::Verbs(e)));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    }

    /// Access the underlying CQ.
    pub fn cq(&self) -> &Cq {
        &self.cq
    }

    /// Drain all pending events from the completion channel.
    fn drain_channel_events(&self) -> Result<()> {
        let channel = self.cq.channel().expect("channel-backed CQ");
        loop {
            match channel.get_cq_event() {
                Ok(_cq_ptr) => {
                    let count = self.unacked_events.fetch_add(1, Ordering::Relaxed) + 1;
                    if count >= ACK_BATCH_SIZE {
                        self.ack_events(count);
                    }
                }
                Err(crate::Error::WouldBlock) => return Ok(()),
                Err(e) => {
                    // Non-blocking read returned a real error
                    let io_err = match e {
                        crate::Error::Verbs(io_err) => io_err,
                        _ => io::Error::other(e.to_string()),
                    };
                    if io_err.kind() == io::ErrorKind::WouldBlock {
                        return Ok(());
                    }
                    return Err(Error::Verbs(io_err));
                }
            }
        }
    }

    /// Batch-ack CQ events.
    fn ack_events(&self, count: u32) {
        self.unacked_events.store(0, Ordering::Relaxed);
        unsafe {
            rdma_io_sys::ibverbs::ibv_ack_cq_events(self.cq.inner().as_raw(), count);
        }
    }
}

impl<N: CqNotifier> Drop for Completions<N> {
    fn drop(&mut self) {
        // Drain any remaining channel events
        if self.cq.has_channel() {
            let _ = self.drain_channel_events();
        }
        // Ack any unacked events
        let remaining = self.unacked_events.load(Ordering::Relaxed);
        if remaining > 0 {
            self.ack_events(remaining);
        }
    }
}
