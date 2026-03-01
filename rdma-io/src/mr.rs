//! Memory Region.

use std::sync::Arc;

use rdma_io_sys::ibverbs::*;

use crate::pd::ProtectionDomain;

bitflags::bitflags! {
    /// Memory access flags for memory registration.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AccessFlags: u32 {
        const LOCAL_WRITE = IBV_ACCESS_LOCAL_WRITE;
        const REMOTE_WRITE = IBV_ACCESS_REMOTE_WRITE;
        const REMOTE_READ = IBV_ACCESS_REMOTE_READ;
        const REMOTE_ATOMIC = IBV_ACCESS_REMOTE_ATOMIC;
        const MW_BIND = IBV_ACCESS_MW_BIND;
        const ZERO_BASED = IBV_ACCESS_ZERO_BASED;
        const ON_DEMAND = IBV_ACCESS_ON_DEMAND;
        const HUGETLB = IBV_ACCESS_HUGETLB;
        const RELAXED_ORDERING = IBV_ACCESS_RELAXED_ORDERING;
    }
}

/// A borrowed memory region (`ibv_mr`).
///
/// Borrows the user buffer for its lifetime `'a` and keeps the PD alive
/// via `Arc`.
pub struct MemoryRegion<'a> {
    pub(crate) inner: *mut ibv_mr,
    pub(crate) _pd: Arc<ProtectionDomain>,
    pub(crate) _lifetime: std::marker::PhantomData<&'a mut [u8]>,
}

// Safety: ibv_mr is thread-safe once registered.
unsafe impl Send for MemoryRegion<'_> {}
unsafe impl Sync for MemoryRegion<'_> {}

impl Drop for MemoryRegion<'_> {
    fn drop(&mut self) {
        let ret = unsafe { ibv_dereg_mr(self.inner) };
        if ret != 0 {
            tracing::error!("ibv_dereg_mr failed: {}", std::io::Error::from_raw_os_error(-ret));
        }
    }
}

impl MemoryRegion<'_> {
    /// The local key for this MR.
    pub fn lkey(&self) -> u32 {
        unsafe { (*self.inner).lkey }
    }

    /// The remote key for this MR.
    pub fn rkey(&self) -> u32 {
        unsafe { (*self.inner).rkey }
    }

    /// The registered address.
    pub fn addr(&self) -> *mut u8 {
        unsafe { (*self.inner).addr.cast() }
    }

    /// The registered length.
    pub fn length(&self) -> usize {
        unsafe { (*self.inner).length as usize }
    }

    /// Raw pointer (for advanced/FFI use).
    pub fn as_raw(&self) -> *mut ibv_mr {
        self.inner
    }
}

/// An owned memory region.
///
/// The buffer is owned by this struct and deregistered + freed on drop.
pub struct OwnedMemoryRegion {
    pub(crate) inner: *mut ibv_mr,
    pub(crate) _pd: Arc<ProtectionDomain>,
    pub(crate) _buf: Box<[u8]>,
}

// Safety: ibv_mr is thread-safe once registered.
unsafe impl Send for OwnedMemoryRegion {}
unsafe impl Sync for OwnedMemoryRegion {}

impl Drop for OwnedMemoryRegion {
    fn drop(&mut self) {
        // Deregister MR first, then buffer is freed when _buf drops.
        let ret = unsafe { ibv_dereg_mr(self.inner) };
        if ret != 0 {
            tracing::error!("ibv_dereg_mr failed: {}", std::io::Error::from_raw_os_error(-ret));
        }
    }
}

impl OwnedMemoryRegion {
    /// The local key.
    pub fn lkey(&self) -> u32 {
        unsafe { (*self.inner).lkey }
    }

    /// The remote key.
    pub fn rkey(&self) -> u32 {
        unsafe { (*self.inner).rkey }
    }

    /// Access the registered buffer.
    pub fn as_slice(&self) -> &[u8] {
        &self._buf
    }

    /// Mutably access the registered buffer.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self._buf
    }

    /// Raw pointer (for advanced/FFI use).
    pub fn as_raw(&self) -> *mut ibv_mr {
        self.inner
    }
}
