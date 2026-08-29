//! V2 device context facade.
//!
//! Provides ergonomic device discovery and context management,
//! wrapping the lower-level [`crate::device`] API with simplified
//! constructors and automatic error handling.

use std::sync::Arc;

use crate::cm::CmId;
use crate::device;

use super::error::{Error, Result};
use super::pd::Pd;

/// An opened RDMA device context.
///
/// `Context` is the entry point for the v2 API. It wraps an opened
/// RDMA device and provides methods to allocate child resources
/// (protection domains, completion queues, etc.).
///
/// # Construction
///
/// Use [`Context::open_first()`] for quick setup or
/// [`Context::open_by_name()`] when targeting a specific device.
/// For CM-based connections, use [`Context::from_cm()`] to obtain
/// the context from an established CM connection.
///
/// # Thread Safety
///
/// `Context` is `Send + Sync` and can be shared across threads.
/// The inner device context uses reference counting to ensure
/// proper cleanup.
#[derive(Clone)]
pub struct Context {
    inner: Arc<device::Context>,
}

impl Context {
    /// Open the first available RDMA device.
    ///
    /// Returns an error if no RDMA devices are found on the system.
    ///
    /// # Errors
    ///
    /// - [`Error::NoDevices`] if no RDMA devices are available
    /// - [`Error::Verbs`] if the device cannot be opened
    pub fn open_first() -> Result<Self> {
        let ctx = device::open_first_device()?;
        Ok(Self {
            inner: Arc::new(ctx),
        })
    }

    /// Open an RDMA device by its kernel name (e.g., `"rxe0"`, `"mlx5_0"`).
    ///
    /// # Errors
    ///
    /// - [`Error::DeviceNotFound`] if no device with that name exists
    /// - [`Error::Verbs`] if the device cannot be opened
    pub fn open_by_name(name: &str) -> Result<Self> {
        let ctx = device::open_device_by_name(name)?;
        Ok(Self {
            inner: Arc::new(ctx),
        })
    }

    /// Obtain a context from a CM connection.
    ///
    /// This wraps the CM-owned verbs context, enabling resource creation
    /// (PD, CQ, MR) from a connection established through RDMA CM.
    /// The typical flow is:
    ///
    /// 1. Establish a CM connection (resolve address, route)
    /// 2. Call `Context::from_cm(&cm_id)` to get the device context
    /// 3. Allocate PD and CQs from this context
    /// 4. Build a QP using those resources
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if the CM ID has no verbs context
    ///   (address not yet resolved)
    pub fn from_cm(cm_id: &CmId) -> Result<Self> {
        let ctx = cm_id.verbs_context().ok_or_else(|| {
            Error::InvalidConfig("CM ID has no verbs context (resolve_addr first)".into())
        })?;
        Ok(Self { inner: ctx })
    }

    /// Allocate a protection domain from this context.
    ///
    /// The protection domain scopes memory registrations and queue pairs.
    ///
    /// # Errors
    ///
    /// - [`Error::Verbs`] if PD allocation fails
    pub fn alloc_pd(&self) -> Result<Pd> {
        let pd = crate::pd::ProtectionDomain::new(Arc::clone(&self.inner))?;
        Ok(Pd::new(pd))
    }

    /// Access the underlying device context.
    ///
    /// Use this for interop with the v1 API or advanced operations
    /// not covered by the v2 facade.
    pub fn inner(&self) -> &Arc<device::Context> {
        &self.inner
    }

    /// Create from an existing `Arc<Context>`.
    ///
    /// Useful for wrapping a verbs context obtained from other sources
    /// (e.g., `AsyncCmId::verbs_context()`).
    pub fn from_inner(ctx: Arc<device::Context>) -> Self {
        Self { inner: ctx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_first_with_device() {
        // This test requires an RDMA device (e.g. rxe0)
        match Context::open_first() {
            Ok(ctx) => {
                // Should be able to allocate a PD
                let pd = ctx.alloc_pd();
                assert!(pd.is_ok(), "PD allocation should succeed");
            }
            Err(Error::NoDevices) => {
                // No device available — skip
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn test_open_by_name_not_found() {
        let result = Context::open_by_name("nonexistent_device_12345");
        assert!(matches!(result, Err(Error::DeviceNotFound(_))));
    }
}
