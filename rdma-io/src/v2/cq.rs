//! V2 completion queue with builder pattern and dual CQ integration models.
//!
//! Supports both direct RDMA CQ polling (no completion channel) and
//! fd/readiness-based CQ notification (with completion channel) for
//! integration with Rust async runtimes.

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
    /// - [`Error::InvalidConfig`](super::error::Error::InvalidConfig) if
    ///   `cqe` is less than 1
    /// - [`Error::Verbs`](super::error::Error::Verbs) if CQ or completion
    ///   channel creation fails
    pub fn build(self) -> Result<Cq> {
        if self.cqe < 1 {
            return Err(super::error::Error::InvalidConfig(
                "CQ capacity (cqe) must be >= 1".into(),
            ));
        }
        let inner_ctx = Arc::clone(self.ctx.inner());
        if self.use_channel {
            let channel = Arc::new(CompletionChannel::new(&inner_ctx)?);
            let cq = CompletionQueue::with_comp_channel(inner_ctx, self.cqe, &channel)?;
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

/// An RDMA completion queue supporting both direct CQ polling and
/// fd/readiness-based notification for Rust async runtimes.
///
/// Created via [`CqBuilder`]. The CQ integration model is determined at
/// construction time:
///
/// - **Poll-only** (no channel): Use [`Cq::poll()`] to directly poll the
///   RDMA CQ, consistent with v1 CQ polling behavior.
/// - **Channel-backed**: Use [`Cq::fd()`] to obtain the completion channel
///   file descriptor for registration with a Rust async runtime's reactor,
///   then use [`Completions`](super::Completions) for async CQ draining.
///
/// # Thread Safety
///
/// `Cq` is `Send + Sync`. However, concurrent polling from multiple
/// threads requires external synchronization — each `poll()` call
/// consumes completions that other callers would miss.
pub struct Cq {
    inner: Arc<CompletionQueue>,
    /// Completion channel for readiness notification (Arc-shared so that
    /// `ConnectionLifetime` can hold a reference that outlives the driver,
    /// ensuring `ibv_destroy_comp_channel` runs only after all CQ refs
    /// are gone — see the drop-order proof in `connection.rs`).
    channel: Option<Arc<CompletionChannel>>,
}

impl Cq {
    /// Poll the RDMA completion queue for completed operations.
    ///
    /// Directly polls the underlying CQ (consistent with v1 CQ polling
    /// behavior). Fills `completions` with up to `completions.len()`
    /// entries and returns the number of completions retrieved.
    ///
    /// Returns `Ok(0)` when no completions are pending (non-blocking).
    ///
    /// # Errors
    ///
    /// - [`Error::Verbs`](super::error::Error::Verbs) if the underlying poll
    ///   operation fails
    pub fn poll(&self, completions: &mut [WorkCompletion]) -> Result<usize> {
        let n = self.inner.poll(completions)?;
        Ok(n)
    }

    /// Get the completion channel file descriptor.
    ///
    /// Returns `Some(fd)` if this CQ was created with a completion channel
    /// (via [`CqBuilder::with_channel()`]), `None` for poll-only CQs.
    ///
    /// Used internally by [`Completions`](super::Completions) and
    /// [`CqNotifier`](super::CqNotifier) implementations to register
    /// with the async runtime's reactor. Exposed for custom notifier
    /// implementations.
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
        self.channel.as_deref()
    }

    /// Access the underlying completion queue.
    ///
    /// Use this for interop with the v1 API or advanced operations.
    pub fn inner(&self) -> &Arc<CompletionQueue> {
        &self.inner
    }
}
