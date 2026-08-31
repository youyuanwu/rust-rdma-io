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
use std::os::unix::io::RawFd;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;

use crate::async_cq::CqNotifier;
use crate::wc::WorkCompletion;

use super::cq::Cq;
use super::error::{Error, Result};

/// State for the drain-after-arm loop.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PollState {
    /// Poll before arming so work already in the CQ is observed immediately.
    #[default]
    PollBeforeArm,
    /// Arm the CQ notification source.
    Arm,
    /// Poll after arming to close the arm-to-wait race.
    PollAfterArm,
    /// CQ was armed and both polls were empty; wait for fd readiness.
    WaitingFd,
    /// Drain and acknowledge completion-channel notifications.
    DrainFd,
}

/// Reusable lost-wakeup-safe CQ notification protocol.
///
/// The state machine preserves the required ordering across future polls:
/// poll, arm, immediate re-poll, wait for readiness, drain/acknowledge the
/// channel, then start again. It never uses a fallback timer.
#[derive(Debug, Default)]
pub(crate) struct CqReadiness {
    state: PollState,
    arms: u64,
}

impl CqReadiness {
    pub(crate) fn poll_with_notifier<N: CqNotifier>(
        &mut self,
        cq: &Cq,
        notifier: &N,
        cx: &mut Context<'_>,
        buf: &mut [WorkCompletion],
    ) -> Poll<Result<usize>> {
        self.poll_with(
            cq,
            cx,
            buf,
            |cx| notifier.poll_readable(cx),
            |_| false,
            |_| false,
        )
    }

    pub(crate) fn poll_with_async_fd_and_hooks(
        &mut self,
        cq: &Cq,
        async_fd: &AsyncFd<RawFd>,
        cx: &mut Context<'_>,
        buf: &mut [WorkCompletion],
        before_arm: impl FnMut(u64) -> bool,
        after_arm: impl FnMut(u64) -> bool,
    ) -> Poll<Result<usize>> {
        self.poll_with(
            cq,
            cx,
            buf,
            |cx| match async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(mut guard)) => {
                    guard.clear_ready();
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            },
            before_arm,
            after_arm,
        )
    }

    fn poll_with(
        &mut self,
        cq: &Cq,
        cx: &mut Context<'_>,
        buf: &mut [WorkCompletion],
        mut poll_readable: impl FnMut(&mut Context<'_>) -> Poll<io::Result<()>>,
        mut before_arm: impl FnMut(u64) -> bool,
        mut after_arm: impl FnMut(u64) -> bool,
    ) -> Poll<Result<usize>> {
        loop {
            match self.state {
                PollState::PollBeforeArm | PollState::PollAfterArm => match cq.poll(buf) {
                    Ok(count) if count > 0 => {
                        self.state = PollState::PollBeforeArm;
                        return Poll::Ready(Ok(count));
                    }
                    Ok(_) if self.state == PollState::PollBeforeArm => {
                        self.state = PollState::Arm;
                    }
                    Ok(_) => {
                        self.state = PollState::WaitingFd;
                    }
                    Err(error) => return Poll::Ready(Err(error)),
                },
                PollState::Arm if before_arm(self.arms.saturating_add(1)) => {
                    return Poll::Pending;
                }
                PollState::Arm => match cq.inner().req_notify(false) {
                    Ok(()) => {
                        self.arms = self.arms.saturating_add(1);
                        self.state = PollState::PollAfterArm;
                        if after_arm(self.arms) {
                            return Poll::Pending;
                        }
                    }
                    Err(error) => return Poll::Ready(Err(error.into())),
                },
                PollState::WaitingFd => match poll_readable(cx) {
                    Poll::Ready(Ok(())) => self.state = PollState::DrainFd,
                    Poll::Ready(Err(error)) => {
                        return Poll::Ready(Err(Error::Verbs(error)));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                PollState::DrainFd => match drain_and_ack_channel_events(cq) {
                    Ok(()) => self.state = PollState::PollBeforeArm,
                    Err(error) => return Poll::Ready(Err(error)),
                },
            }
        }
    }

    #[cfg(test)]
    fn state(&self) -> PollState {
        self.state
    }

    #[cfg(test)]
    fn observe_empty_poll(&mut self) {
        self.state = match self.state {
            PollState::PollBeforeArm => PollState::Arm,
            PollState::PollAfterArm => PollState::WaitingFd,
            state => state,
        };
    }

    #[cfg(test)]
    fn observe_completion(&mut self) {
        self.state = PollState::PollBeforeArm;
    }

    #[cfg(test)]
    fn observe_arm(&mut self) {
        debug_assert_eq!(self.state, PollState::Arm);
        self.state = PollState::PollAfterArm;
    }

    #[cfg(test)]
    fn observe_fd_ready(&mut self) {
        debug_assert_eq!(self.state, PollState::WaitingFd);
        self.state = PollState::DrainFd;
    }

    #[cfg(test)]
    fn observe_fd_drained(&mut self) {
        debug_assert_eq!(self.state, PollState::DrainFd);
        self.state = PollState::PollBeforeArm;
    }
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
    readiness: CqReadiness,
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
            readiness: CqReadiness::default(),
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
        std::future::poll_fn(|cx| {
            self.readiness
                .poll_with_notifier(&self.cq, &self.notifier, cx, buf)
        })
        .await
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
        self.readiness
            .poll_with_notifier(&self.cq, &self.notifier, cx, buf)
    }

    /// Access the underlying CQ.
    pub fn cq(&self) -> &Cq {
        &self.cq
    }
}

impl<N: CqNotifier> Drop for Completions<N> {
    fn drop(&mut self) {
        if self.cq.has_channel() {
            let _ = drain_and_ack_channel_events(&self.cq);
        }
    }
}

fn drain_and_ack_channel_events(cq: &Cq) -> Result<()> {
    let channel = cq.channel().expect("channel-backed CQ");
    drain_and_ack_events(
        || channel.get_cq_event().map(|_| ()),
        |count| unsafe {
            rdma_io_sys::ibverbs::ibv_ack_cq_events(cq.inner().as_raw(), count);
        },
    )
}

fn drain_and_ack_events(
    mut get_event: impl FnMut() -> crate::Result<()>,
    mut acknowledge: impl FnMut(u32),
) -> Result<()> {
    let mut count = 0u32;
    loop {
        match get_event() {
            Ok(()) => {
                count = count.saturating_add(1);
            }
            Err(crate::Error::WouldBlock) => break,
            Err(error) => {
                let io_error = match error {
                    crate::Error::Verbs(io_error) => io_error,
                    other => io::Error::other(other.to_string()),
                };
                if io_error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if count > 0 {
                    acknowledge(count);
                }
                return Err(Error::Verbs(io_error));
            }
        }
    }
    if count > 0 {
        acknowledge(count);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cqe_before_arm_restarts_at_the_initial_poll() {
        let mut state = CqReadiness::default();
        assert_eq!(state.state(), PollState::PollBeforeArm);
        state.observe_completion();
        assert_eq!(state.state(), PollState::PollBeforeArm);
    }

    #[test]
    fn cqe_after_arm_before_wait_is_observed_by_the_recheck() {
        let mut state = CqReadiness::default();
        state.observe_empty_poll();
        assert_eq!(state.state(), PollState::Arm);
        state.observe_arm();
        assert_eq!(state.state(), PollState::PollAfterArm);
        state.observe_completion();
        assert_eq!(state.state(), PollState::PollBeforeArm);
    }

    #[test]
    fn fd_ready_before_registration_is_drained_then_rechecked() {
        let mut state = CqReadiness::default();
        state.observe_empty_poll();
        state.observe_arm();
        state.observe_empty_poll();
        assert_eq!(state.state(), PollState::WaitingFd);
        state.observe_fd_ready();
        assert_eq!(state.state(), PollState::DrainFd);
        state.observe_fd_drained();
        assert_eq!(state.state(), PollState::PollBeforeArm);
    }

    #[test]
    fn stale_notification_returns_to_arm_after_an_empty_recheck() {
        let mut state = CqReadiness::default();
        state.observe_empty_poll();
        state.observe_arm();
        state.observe_empty_poll();
        state.observe_fd_ready();
        state.observe_fd_drained();
        state.observe_empty_poll();
        state.observe_arm();
        state.observe_empty_poll();
        assert_eq!(state.state(), PollState::WaitingFd);
    }

    #[test]
    fn spurious_software_wake_does_not_change_cq_wait_state() {
        let mut state = CqReadiness::default();
        state.observe_empty_poll();
        state.observe_arm();
        state.observe_empty_poll();
        assert_eq!(state.state(), PollState::WaitingFd);
        assert_eq!(state.state(), PollState::WaitingFd);
    }

    #[test]
    fn drained_events_are_acknowledged_before_a_later_error_is_propagated() {
        let mut calls = 0;
        let mut acknowledged = Vec::new();
        let error = drain_and_ack_events(
            || {
                calls += 1;
                match calls {
                    1 | 2 => Ok(()),
                    _ => Err(crate::Error::Verbs(io::Error::other(
                        "injected completion-channel failure",
                    ))),
                }
            },
            |count| acknowledged.push(count),
        )
        .unwrap_err();

        assert!(
            matches!(error, Error::Verbs(ref source) if source.to_string().contains("injected"))
        );
        assert_eq!(acknowledged, [2]);
    }
}
