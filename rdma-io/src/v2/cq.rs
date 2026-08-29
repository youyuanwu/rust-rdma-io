//! V2 completion queue with builder pattern and dual completion models.
//!
//! Supports both polling-based completion draining (no completion channel)
//! and readiness-based notification (with completion channel and fd exposure).

use std::os::unix::io::RawFd;
use std::sync::Arc;

use crate::comp_channel::CompletionChannel;
use crate::cq::CompletionQueue;
use crate::wc::WorkCompletion;

use super::context::Context;
use super::error::Result;

/// Builder for creating completion queues with explicit configuration.
///
/// # Examples
///
/// ```no_run
/// # use rdma_io::v2::*;
/// # fn example() -> Result<()> {
/// let ctx = Context::open_first()?;
///
/// // Poll-only CQ (no completion channel)
/// let poll_cq = CqBuilder::new(&ctx, 32).build()?;
///
/// // Channel-backed CQ (for readiness/async use)
/// let async_cq = CqBuilder::new(&ctx, 32).with_channel().build()?;
/// # Ok(())
/// # }
/// ```
pub struct CqBuilder<'a> {
    ctx: &'a Context,
    cqe: i32,
    use_channel: bool,
}

impl<'a> CqBuilder<'a> {
    /// Create a new CQ builder.
    ///
    /// `cqe` specifies the minimum number of completion entries the CQ
    /// can hold. The actual capacity may be larger.
    pub fn new(ctx: &'a Context, cqe: i32) -> Self {
        Self {
            ctx,
            cqe,
            use_channel: false,
        }
    }

    /// Enable a completion channel for readiness-based notification.
    ///
    /// When enabled, the resulting [`Cq`] will expose an fd via [`Cq::fd()`]
    /// suitable for registration with event loops or async runtimes.
    pub fn with_channel(mut self) -> Self {
        self.use_channel = true;
        self
    }

    /// Build the completion queue.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if `cqe` is less than 1
    /// - [`Error::Verbs`] if CQ or completion channel creation fails
    pub fn build(self) -> Result<Cq> {
        if self.cqe < 1 {
            return Err(super::error::Error::InvalidConfig(
                "CQ capacity (cqe) must be >= 1".into(),
            ));
        }
        let inner_ctx = Arc::clone(self.ctx.inner());
        if self.use_channel {
            let channel = CompletionChannel::new(&inner_ctx)?;
            let cq = CompletionQueue::with_comp_channel(
                inner_ctx,
                self.cqe,
                &channel,
            )?;
            Ok(Cq {
                inner: cq,
                channel: Some(channel),
            })
        } else {
            let cq = CompletionQueue::new(inner_ctx, self.cqe)?;
            Ok(Cq {
                inner: cq,
                channel: None,
            })
        }
    }
}

/// An RDMA completion queue supporting both polling and readiness models.
///
/// Created via [`CqBuilder`]. The completion model is determined at
/// construction time:
///
/// - **Poll-only** (no channel): Use [`Cq::poll()`] to drain completions
///   in a busy loop or custom event loop.
/// - **Channel-backed**: Use [`Cq::fd()`] to obtain a file descriptor
///   for event-loop registration, or use async completion integration.
///
/// # Thread Safety
///
/// `Cq` is `Send + Sync`. However, concurrent polling from multiple
/// threads requires external synchronization — each `poll()` call
/// consumes completions that other callers would miss.
pub struct Cq {
    inner: Arc<CompletionQueue>,
    channel: Option<CompletionChannel>,
}

impl Cq {
    /// Poll the completion queue for completed operations.
    ///
    /// Fills `completions` with up to `completions.len()` entries and
    /// returns the number of completions retrieved.
    ///
    /// Returns `Ok(0)` when no completions are pending (non-blocking).
    ///
    /// # Errors
    ///
    /// - [`Error::Verbs`] if the underlying poll operation fails
    pub fn poll(&self, completions: &mut [WorkCompletion]) -> Result<usize> {
        let n = self.inner.poll(completions)?;
        Ok(n)
    }

    /// Get the file descriptor for readiness-based notification.
    ///
    /// Returns `Some(fd)` if this CQ was created with a completion channel
    /// (via [`CqBuilder::with_channel()`]), `None` for poll-only CQs.
    ///
    /// The fd becomes readable when new completions are available.
    /// Register it with epoll, poll, or an async runtime's reactor.
    pub fn fd(&self) -> Option<RawFd> {
        self.channel.as_ref().map(|ch| ch.fd())
    }

    /// Check whether this CQ has a completion channel.
    pub fn has_channel(&self) -> bool {
        self.channel.is_some()
    }

    /// Access the completion channel, if present.
    ///
    /// Useful for advanced integration patterns or interop with
    /// the v1 async API.
    pub fn channel(&self) -> Option<&CompletionChannel> {
        self.channel.as_ref()
    }

    /// Access the underlying completion queue.
    ///
    /// Use this for interop with the v1 API or advanced operations.
    pub fn inner(&self) -> &Arc<CompletionQueue> {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::error::Error;
    use super::super::context::Context;

    #[test]
    fn test_cq_builder_poll_only() {
        match Context::open_first() {
            Ok(ctx) => {
                let cq = CqBuilder::new(&ctx, 16).build();
                assert!(cq.is_ok());
                let cq = cq.unwrap();
                assert!(!cq.has_channel());
                assert!(cq.fd().is_none());

                // Polling empty CQ should return 0
                let mut completions = [WorkCompletion::default(); 4];
                let n = cq.poll(&mut completions).unwrap();
                assert_eq!(n, 0);
            }
            Err(Error::NoDevices) => {} // skip
            Err(e) => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn test_cq_builder_with_channel() {
        match Context::open_first() {
            Ok(ctx) => {
                let cq = CqBuilder::new(&ctx, 16).with_channel().build();
                assert!(cq.is_ok());
                let cq = cq.unwrap();
                assert!(cq.has_channel());
                assert!(cq.fd().is_some());
                // fd should be positive
                assert!(cq.fd().unwrap() >= 0);
            }
            Err(Error::NoDevices) => {} // skip
            Err(e) => panic!("unexpected: {e}"),
        }
    }
}
