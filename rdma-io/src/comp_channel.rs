//! Completion Channel for async CQ notification.
//!
//! Wraps `ibv_comp_channel` which provides a file descriptor that becomes
//! readable when an associated CQ has a completion event. This fd can be
//! registered with an async runtime's reactor (epoll/kqueue) for non-blocking
//! CQ notification.

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use rdma_io_sys::ibverbs::*;

use crate::Result;
use crate::device::Context;
use crate::error::from_ptr;

/// Safe wrapper around `ibv_comp_channel`.
///
/// Provides the file descriptor used for async CQ notification.
/// The fd is set to non-blocking mode at creation time.
pub struct CompletionChannel {
    inner: *mut ibv_comp_channel,
    _ctx: Arc<Context>,
}

// Safety: ibv_comp_channel is usable from any thread. The fd is a kernel resource.
unsafe impl Send for CompletionChannel {}
unsafe impl Sync for CompletionChannel {}

impl CompletionChannel {
    /// Create a new completion channel with non-blocking fd.
    pub fn new(ctx: &Arc<Context>) -> Result<Self> {
        let ch = from_ptr(unsafe { ibv_create_comp_channel(ctx.inner) })?;

        // Set non-blocking for async use
        let fd = unsafe { (*ch).fd };
        set_nonblocking(fd)?;

        Ok(Self {
            inner: ch,
            _ctx: Arc::clone(ctx),
        })
    }

    /// Raw file descriptor (for async reactor registration).
    pub fn fd(&self) -> RawFd {
        unsafe { (*self.inner).fd }
    }

    /// Raw pointer (for CQ creation).
    pub(crate) fn as_raw(&self) -> *mut ibv_comp_channel {
        self.inner
    }

    /// Consume one CQ event notification (non-blocking).
    ///
    /// Returns the raw CQ pointer that fired. The caller must call
    /// `ibv_ack_cq_events` later to acknowledge consumed events.
    ///
    /// Returns `WouldBlock` if no event is available (fd is non-blocking).
    pub fn get_cq_event(&self) -> Result<*mut ibv_cq> {
        let mut cq: *mut ibv_cq = std::ptr::null_mut();
        let mut ctx: *mut core::ffi::c_void = std::ptr::null_mut();
        let ret = unsafe { ibv_get_cq_event(self.inner, &mut cq, &mut ctx) };
        if ret != 0 {
            Err(crate::Error::Verbs(io::Error::last_os_error()))
        } else {
            Ok(cq)
        }
    }
}

impl AsRawFd for CompletionChannel {
    fn as_raw_fd(&self) -> RawFd {
        self.fd()
    }
}

impl Drop for CompletionChannel {
    fn drop(&mut self) {
        let ret = unsafe { ibv_destroy_comp_channel(self.inner) };
        if ret != 0 {
            tracing::error!(
                "ibv_destroy_comp_channel failed: {}",
                io::Error::from_raw_os_error(-ret)
            );
        }
    }
}

/// Set a file descriptor to non-blocking mode.
fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(crate::Error::Verbs(io::Error::last_os_error()));
    }
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(crate::Error::Verbs(io::Error::last_os_error()));
    }
    Ok(())
}
