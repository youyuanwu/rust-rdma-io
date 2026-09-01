//! Tokio-specific convenience for v2 async completions.

use crate::tokio_notifier::TokioCqNotifier;

use super::completion::Completions;
use super::cq::Cq;
use super::error::{Error, Result};

/// Type alias for Tokio-backed async completions.
///
/// # Use case
///
/// Name the canonical Tokio specialization of [`Completions`].
///
/// # Ownership and progress
///
/// The alias owns the CQ/notifier through `Completions`; applications own the
/// Tokio task that awaits it.
///
/// # Safety and limits
///
/// The CQ must be channel-backed and have one logical consumer.
///
/// # Availability
///
/// Available with the `tokio` feature.
pub type TokioCompletions = Completions<TokioCqNotifier>;

impl Cq {
    /// Create a Tokio-backed async completions poller.
    ///
    /// Convenience method that creates a [`TokioCqNotifier`] from this
    /// CQ's completion channel fd and wraps it in [`Completions`].
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if this CQ has no completion channel
    /// - [`Error::Verbs`] if the Tokio async fd registration fails
    pub fn completions_tokio(self) -> Result<TokioCompletions> {
        let fd = self.fd().ok_or_else(|| {
            Error::InvalidConfig("completions_tokio requires a channel-backed CQ".into())
        })?;
        let notifier = TokioCqNotifier::new(fd).map_err(Error::Verbs)?;
        Completions::new(self, notifier)
    }
}
