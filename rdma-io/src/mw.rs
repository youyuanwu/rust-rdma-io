//! Memory Window — RAII wrapper for `ibv_mw`.
//!
//! Memory Windows provide fine-grained remote access control. A Type 2 MW
//! can be bound to a sub-region of a Memory Region, giving remote peers
//! scoped write access to only the bound range.

use std::sync::Arc;

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::*;

use crate::error::from_ptr;
use crate::pd::ProtectionDomain;

/// Memory Window type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwType {
    Type1 = 1,
    Type2 = 2,
}

impl MwType {
    /// Convert to the raw `ibv_mw_type` constant.
    pub fn as_raw(self) -> u32 {
        match self {
            Self::Type1 => IBV_MW_TYPE_1,
            Self::Type2 => IBV_MW_TYPE_2,
        }
    }
}

/// RAII wrapper for an RDMA Memory Window (`ibv_mw`).
///
/// Allocated via [`MemoryWindow::alloc`] and deallocated on [`Drop`].
/// The PD is kept alive via `Arc` reference.
pub struct MemoryWindow {
    inner: *mut ibv_mw,
    _pd: Arc<ProtectionDomain>,
}

// Safety: ibv_mw is a kernel-managed handle; safe to send/share across threads.
unsafe impl Send for MemoryWindow {}
unsafe impl Sync for MemoryWindow {}

impl MemoryWindow {
    /// Allocate a new Memory Window of the given type.
    pub fn alloc(pd: &Arc<ProtectionDomain>, mw_type: MwType) -> crate::Result<Self> {
        let mw = from_ptr(unsafe { rdma_wrap_ibv_alloc_mw(pd.as_raw(), mw_type.as_raw()) })?;
        Ok(Self {
            inner: mw,
            _pd: Arc::clone(pd),
        })
    }

    /// Remote key for this MW (valid after bind).
    pub fn rkey(&self) -> u32 {
        unsafe { (*self.inner).rkey }
    }

    /// Access the raw `ibv_mw` pointer.
    pub fn as_raw(&self) -> *mut ibv_mw {
        self.inner
    }
}

impl Drop for MemoryWindow {
    fn drop(&mut self) {
        let ret = unsafe { rdma_wrap_ibv_dealloc_mw(self.inner) };
        if ret != 0 {
            tracing::error!("ibv_dealloc_mw failed: {}", std::io::Error::last_os_error());
        }
    }
}

impl std::fmt::Debug for MemoryWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryWindow")
            .field("rkey", &self.rkey())
            .field("ptr", &self.inner)
            .finish()
    }
}
