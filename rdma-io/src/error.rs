//! Error types for the RDMA safe API.

use std::io;

/// Result type alias using [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by RDMA operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An ibverbs call returned an error (typically sets `errno`).
    #[error("ibverbs error: {0}")]
    Verbs(#[from] io::Error),

    /// No RDMA devices found on this system.
    #[error("no RDMA devices found")]
    NoDevices,

    /// The requested device was not found.
    #[error("device not found: {0}")]
    DeviceNotFound(String),

    /// An invalid argument was supplied.
    #[error("invalid argument: {0}")]
    InvalidArg(String),

    /// A work completion finished with an error status.
    #[error("work completion error: {status} (vendor_err={vendor_err:#x})")]
    WorkCompletion {
        /// The WC status code.
        status: u32,
        /// Vendor-specific error code.
        vendor_err: u32,
    },

    /// Non-blocking operation would block (EAGAIN/EWOULDBLOCK).
    #[error("operation would block")]
    WouldBlock,
}

/// Convert a C return code (0 = success, negative = -errno) to a `Result`.
pub(crate) fn from_ret(ret: i32) -> Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Verbs(io::Error::from_raw_os_error(-ret)))
    }
}

/// Convert a C return code (-1 = failure, errno set) to a `Result`.
///
/// Used for rdma_cm functions which return -1 on error and set `errno`,
/// unlike ibverbs functions which return negative errno values directly.
pub(crate) fn from_ret_errno(ret: i32) -> Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(Error::Verbs(io::Error::last_os_error()))
    }
}

/// Convert a nullable pointer return to `Result`, using `errno` on failure.
pub(crate) fn from_ptr<T>(ptr: *mut T) -> Result<*mut T> {
    if ptr.is_null() {
        Err(Error::Verbs(io::Error::last_os_error()))
    } else {
        Ok(ptr)
    }
}
