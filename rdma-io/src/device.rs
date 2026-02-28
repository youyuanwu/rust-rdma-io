//! RDMA device enumeration and context.

use std::ffi::CStr;
use std::sync::Arc;

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::rdma_wrap____ibv_query_port;

use crate::error::{from_ptr, from_ret};
use crate::{Error, Result};

/// A discovered RDMA device (not yet opened).
///
/// Obtained from [`devices()`]. The device list is freed when all `Device`
/// values are dropped.
#[derive(Debug)]
pub struct Device {
    list: Arc<DeviceList>,
    index: usize,
}

/// Owns the `ibv_device**` list returned by `ibv_get_device_list`.
#[derive(Debug)]
struct DeviceList {
    list: *mut *mut ibv_device,
    #[allow(dead_code)]
    count: usize,
}

// Safety: ibv_device pointers are process-wide and thread-safe.
unsafe impl Send for DeviceList {}
unsafe impl Sync for DeviceList {}

impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe { ibv_free_device_list(self.list) };
    }
}

/// Enumerate all RDMA devices on the system.
///
/// Returns an empty `Vec` if no devices are found. Use [`open_device`](Device::open)
/// to obtain a [`Context`].
pub fn devices() -> Result<Vec<Device>> {
    let mut num_devices: i32 = 0;
    let list = unsafe { ibv_get_device_list(&mut num_devices) };
    if list.is_null() {
        return Err(Error::Verbs(std::io::Error::last_os_error()));
    }
    let count = num_devices as usize;
    let shared = Arc::new(DeviceList { list, count });
    let devs = (0..count)
        .map(|i| Device {
            list: Arc::clone(&shared),
            index: i,
        })
        .collect();
    Ok(devs)
}

impl Device {
    /// The kernel name of this device (e.g. `"siw0"`, `"mlx5_0"`).
    pub fn name(&self) -> &str {
        let dev = self.as_ptr();
        let name = unsafe { ibv_get_device_name(dev) };
        if name.is_null() {
            "<unknown>"
        } else {
            unsafe { CStr::from_ptr(name) }
                .to_str()
                .unwrap_or("<invalid utf8>")
        }
    }

    /// The node GUID of this device (network byte order).
    pub fn guid(&self) -> u64 {
        unsafe { ibv_get_device_guid(self.as_ptr()) }
    }

    /// Open this device and return a [`Context`].
    pub fn open(&self) -> Result<Context> {
        let ctx = from_ptr(unsafe { ibv_open_device(self.as_ptr()) })?;
        Ok(Context { inner: ctx })
    }

    fn as_ptr(&self) -> *mut ibv_device {
        unsafe { *self.list.list.add(self.index) }
    }
}

/// An opened RDMA device context.
///
/// Wraps `ibv_context*`. Create via [`Device::open`].
/// All child resources (PD, CQ, QP, …) hold an `Arc<Context>` to keep
/// the context alive.
pub struct Context {
    pub(crate) inner: *mut ibv_context,
}

// Safety: ibv_context is thread-safe (protected by internal locking).
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            ibv_close_device(self.inner);
        }
    }
}

impl Context {
    /// Query device attributes.
    pub fn query_device(&self) -> Result<ibv_device_attr> {
        let mut attr = ibv_device_attr::default();
        from_ret(unsafe { ibv_query_device(self.inner, &mut attr) })?;
        Ok(attr)
    }

    /// Query port attributes.
    pub fn query_port(&self, port_num: u8) -> Result<ibv_port_attr> {
        let mut attr = ibv_port_attr::default();
        from_ret(unsafe { rdma_wrap____ibv_query_port(self.inner, port_num, &mut attr) })?;
        Ok(attr)
    }

    /// Query a single GID entry by index.
    pub fn query_gid(&self, port_num: u8, index: i32) -> Result<ibv_gid> {
        let mut gid = ibv_gid::default();
        from_ret(unsafe { ibv_query_gid(self.inner, port_num, index, &mut gid) })?;
        Ok(gid)
    }

    /// Raw `ibv_context` pointer (for advanced/FFI use).
    pub fn as_raw(&self) -> *mut ibv_context {
        self.inner
    }
}

/// Open the first available RDMA device.
///
/// Convenience function: equivalent to `devices()?.first().open()`.
pub fn open_first_device() -> Result<Context> {
    let devs = devices()?;
    if devs.is_empty() {
        return Err(Error::NoDevices);
    }
    devs[0].open()
}

/// Open an RDMA device by name.
pub fn open_device_by_name(name: &str) -> Result<Context> {
    let devs = devices()?;
    for d in &devs {
        if d.name() == name {
            return d.open();
        }
    }
    Err(Error::DeviceNotFound(name.to_string()))
}
