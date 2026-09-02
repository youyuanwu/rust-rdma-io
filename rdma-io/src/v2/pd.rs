//! V2 protection domain wrapper.
//!
//! Wraps [`crate::pd::ProtectionDomain`] with a v2-consistent interface
//! for memory registration and resource creation.

use std::sync::Arc;

use crate::pd::ProtectionDomain;

/// An RDMA protection domain.
///
/// # Use case
///
/// Register typed memory and build queue pairs on an independent V2 context.
///
/// # Ownership and progress
///
/// A `Pd` retains its anchored parent context and owns no progress task.
///
/// # Safety and limits
///
/// Registered memory and queue pairs keep the protection domain alive.
///
/// # Availability
///
/// Created by [`Context::alloc_pd`](super::Context::alloc_pd).
#[derive(Clone)]
pub struct Pd {
    inner: Arc<ProtectionDomain>,
}

impl Pd {
    /// Create a new `Pd` from an existing protection domain.
    pub(crate) fn new(pd: Arc<ProtectionDomain>) -> Self {
        Self { inner: pd }
    }

    pub(crate) fn raw_pd(&self) -> &Arc<ProtectionDomain> {
        &self.inner
    }

    pub(crate) fn raw_context(&self) -> &Arc<crate::device::Context> {
        self.inner.context()
    }
}
