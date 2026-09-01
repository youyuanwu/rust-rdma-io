//! V2 device context facade.
//!
//! Provides owned, librdmacm-anchored device discovery and context management.

use std::sync::Arc;

use crate::cm::RdmaCmDeviceList;
use crate::device;

use super::error::{Error, Result};
use super::pd::Pd;

/// An owned, non-closing RDMA device context facade.
///
/// # Use case
///
/// Use [`Context::open_first`] or [`Context::open_by_name`] to construct
/// independent V2 protection domains, completion queues, and queue pairs.
///
/// # Ownership and progress
///
/// Each context retains the complete list returned by `rdma_get_devices`.
/// Child resources retain the facade transitively. Contexts do not drive
/// progress or create tasks.
///
/// # Safety and limits
///
/// The facade never calls `ibv_close_device`. The retained librdmacm list is
/// released with `rdma_free_devices` only after the facade and all descendants
/// are gone. Repeated same-name opens may refer to librdmacm's cached raw
/// context.
///
/// # Availability
///
/// Device availability and first-device ordering follow librdmacm enumeration.
/// A verbs-openable device absent from that enumeration is unavailable through
/// these constructors.
#[derive(Clone)]
pub struct Context {
    inner: Arc<device::Context>,
}

impl Context {
    /// Open the first librdmacm-enumerated RDMA device.
    ///
    /// Returns an error if no RDMA devices are found on the system.
    ///
    /// # Errors
    ///
    /// - [`Error::NoDevices`] if no RDMA devices are available
    /// - [`Error::Verbs`] if librdmacm enumeration fails
    pub fn open_first() -> Result<Self> {
        let devices = RdmaCmDeviceList::new().map_err(Error::from_v1)?;
        let inner = devices.first_context().map_err(Error::from_v1)?;
        Ok(Self { inner })
    }

    /// Open a librdmacm-enumerated device by kernel name.
    ///
    /// # Errors
    ///
    /// - [`Error::DeviceNotFound`] if no device with that name exists
    /// - [`Error::Verbs`] if librdmacm enumeration fails
    pub fn open_by_name(name: &str) -> Result<Self> {
        let devices = RdmaCmDeviceList::new().map_err(Error::from_v1)?;
        let inner = devices.context_by_name(name).map_err(Error::from_v1)?;
        Ok(Self { inner })
    }

    /// Allocate a protection domain from this context.
    ///
    /// The protection domain scopes memory registrations and queue pairs.
    ///
    /// # Errors
    ///
    /// - [`Error::Verbs`] if PD allocation fails
    pub fn alloc_pd(&self) -> Result<Pd> {
        let pd =
            crate::pd::ProtectionDomain::new(Arc::clone(&self.inner)).map_err(Error::from_v1)?;
        Ok(Pd::new(pd))
    }

    pub(crate) fn raw_context(&self) -> &Arc<device::Context> {
        &self.inner
    }

    pub(crate) fn from_anchored(inner: Arc<device::Context>) -> Self {
        Self { inner }
    }
}
