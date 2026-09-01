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

    #[cfg(feature = "tokio")]
    pub(crate) fn from_anchored(inner: Arc<device::Context>) -> Self {
        Self { inner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_device_name(devices: &RdmaCmDeviceList) -> Option<String> {
        devices.device_names().into_iter().next()
    }

    #[test]
    fn open_first_uses_the_first_librdmacm_context() {
        let Ok(devices) = RdmaCmDeviceList::new() else {
            return;
        };
        let Some(first_name) = first_device_name(&devices) else {
            return;
        };
        let first = Context::open_first().expect("open first librdmacm context");
        let by_name =
            Context::open_by_name(&first_name).expect("open first librdmacm context by name");

        assert_eq!(
            first.raw_context().as_raw(),
            by_name.raw_context().as_raw(),
            "open_first must select the context at librdmacm list index zero"
        );
    }

    #[test]
    fn same_name_openers_share_librdmacm_cached_raw_context() {
        let Ok(devices) = RdmaCmDeviceList::new() else {
            return;
        };
        let Some(name) = first_device_name(&devices) else {
            return;
        };
        let first = Context::open_by_name(&name).expect("first same-name context open");
        let second = Context::open_by_name(&name).expect("second same-name context open");

        assert_eq!(
            first.raw_context().as_raw(),
            second.raw_context().as_raw(),
            "same-name librdmacm opens must preserve the cached raw-context identity"
        );
    }
}
