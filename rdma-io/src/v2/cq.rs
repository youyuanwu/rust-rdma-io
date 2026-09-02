//! V2 completion queue with builder pattern and dual CQ integration models.
//!
//! Supports both direct RDMA CQ polling (no completion channel) and
//! fd/readiness-based CQ notification (with completion channel) for
//! integration with Rust async runtimes.

use std::os::unix::io::RawFd;
use std::sync::Arc;

use super::Completion;
use super::context::Context;
use super::error::Result;
use crate::comp_channel::CompletionChannel;
use crate::cq::CompletionQueue;

/// Builder for creating completion queues with explicit configuration.
///
/// # Use case
///
/// Create either a directly polled CQ or a channel-backed CQ.
///
/// # Ownership and progress
///
/// The resulting CQ retains its anchored context and owns no task.
///
/// # Safety and limits
///
/// The requested entry count must be positive and provider-supported.
///
/// # Availability
///
/// Available in every V2 feature profile.
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
        let inner_ctx = Arc::clone(self.ctx.raw_context());
        if self.use_channel {
            let channel =
                Arc::new(CompletionChannel::new(&inner_ctx).map_err(super::error::Error::from_v1)?);
            let cq = CompletionQueue::with_comp_channel(inner_ctx, self.cqe, &channel)
                .map_err(super::error::Error::from_v1)?;
            Ok(Cq {
                inner: cq,
                channel: Some(channel),
            })
        } else {
            let cq =
                CompletionQueue::new(inner_ctx, self.cqe).map_err(super::error::Error::from_v1)?;
            Ok(Cq {
                inner: cq,
                channel: None,
            })
        }
    }
}

/// An RDMA completion queue supporting direct and readiness-based polling.
///
/// # Use case
///
/// Poll typed [`Completion`] values directly or attach a runtime notifier.
///
/// # Ownership and progress
///
/// The CQ retains its anchored context and optional completion channel. The
/// caller remains the sole completion consumer.
///
/// # Safety and limits
///
/// Concurrent consumers can steal each other's completions and require
/// external synchronization.
///
/// # Availability
///
/// Direct polling is always available; readiness adapters require their
/// corresponding feature.
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
    pub fn poll(&self, completions: &mut [Completion]) -> Result<usize> {
        let n = self
            .inner
            .poll(Completion::raw_slice_mut(completions))
            .map_err(super::error::Error::from_v1)?;
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

    #[cfg(feature = "async")]
    pub(crate) fn completion_channel(&self) -> Option<&CompletionChannel> {
        self.channel.as_deref()
    }

    pub(crate) fn raw_cq(&self) -> &Arc<CompletionQueue> {
        &self.inner
    }

    pub(crate) fn raw_context(&self) -> &Arc<crate::device::Context> {
        self.inner.context()
    }
}
