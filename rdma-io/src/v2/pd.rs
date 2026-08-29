//! V2 protection domain wrapper.
//!
//! Wraps [`crate::pd::ProtectionDomain`] with a v2-consistent interface
//! for memory registration and resource creation.

use std::sync::Arc;

use crate::pd::ProtectionDomain;

/// An RDMA protection domain.
///
/// Scopes memory registrations and queue pairs. Created via
/// [`Context::alloc_pd()`](super::Context::alloc_pd).
///
/// # Thread Safety
///
/// `Pd` is `Send + Sync` and can be shared across threads via cloning
/// (internally reference-counted).
#[derive(Clone)]
pub struct Pd {
    inner: Arc<ProtectionDomain>,
}

impl Pd {
    /// Create a new `Pd` from an existing protection domain.
    pub(crate) fn new(pd: Arc<ProtectionDomain>) -> Self {
        Self { inner: pd }
    }

    /// Access the underlying protection domain.
    ///
    /// Use this for interop with the v1 API or advanced operations.
    pub fn inner(&self) -> &Arc<ProtectionDomain> {
        &self.inner
    }

    /// Access the parent context.
    pub fn context(&self) -> &Arc<crate::device::Context> {
        self.inner.context()
    }
}
