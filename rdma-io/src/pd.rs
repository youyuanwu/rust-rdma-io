//! Protection Domain.

use std::sync::Arc;

use rdma_io_sys::ibverbs::*;

use crate::Result;
use crate::cq::CompletionQueue;
use crate::device::Context;
use crate::error::from_ptr;
use crate::mr::{AccessFlags, MemoryRegion, OwnedMemoryRegion};
use crate::qp::{QpInitAttr, QueuePair};

/// An RDMA Protection Domain (`ibv_pd`).
///
/// All memory registrations and QPs belong to a PD.
pub struct ProtectionDomain {
    pub(crate) inner: *mut ibv_pd,
    pub(crate) ctx: Arc<Context>,
}

// Safety: ibv_pd is thread-safe.
unsafe impl Send for ProtectionDomain {}
unsafe impl Sync for ProtectionDomain {}

impl Drop for ProtectionDomain {
    fn drop(&mut self) {
        unsafe {
            ibv_dealloc_pd(self.inner);
        }
    }
}

impl ProtectionDomain {
    /// Allocate a new PD on the given context.
    pub fn new(ctx: Arc<Context>) -> Result<Arc<Self>> {
        let pd = from_ptr(unsafe { ibv_alloc_pd(ctx.inner) })?;
        Ok(Arc::new(Self { inner: pd, ctx }))
    }

    /// Register a borrowed memory region.
    ///
    /// The returned `MemoryRegion` borrows `buf` and keeps `self` alive via `Arc`.
    pub fn reg_mr<'a>(
        self: &Arc<Self>,
        buf: &'a mut [u8],
        access: AccessFlags,
    ) -> Result<MemoryRegion<'a>> {
        let mr = from_ptr(unsafe {
            ibv_reg_mr(
                self.inner,
                buf.as_mut_ptr().cast(),
                buf.len() as u64,
                access.bits() as i32,
            )
        })?;
        Ok(MemoryRegion {
            inner: mr,
            _pd: Arc::clone(self),
            _lifetime: std::marker::PhantomData,
        })
    }

    /// Register an owned memory region.
    ///
    /// The buffer is moved into the `OwnedMemoryRegion` and freed when it is dropped.
    pub fn reg_mr_owned(
        self: &Arc<Self>,
        buf: Vec<u8>,
        access: AccessFlags,
    ) -> Result<OwnedMemoryRegion> {
        let mut buf = buf.into_boxed_slice();
        let mr = from_ptr(unsafe {
            ibv_reg_mr(
                self.inner,
                buf.as_mut_ptr().cast(),
                buf.len() as u64,
                access.bits() as i32,
            )
        })?;
        Ok(OwnedMemoryRegion {
            inner: mr,
            _pd: Arc::clone(self),
            _buf: buf,
        })
    }

    /// Create a Queue Pair on this PD.
    pub fn create_qp(
        self: &Arc<Self>,
        send_cq: &Arc<CompletionQueue>,
        recv_cq: &Arc<CompletionQueue>,
        init_attr: &QpInitAttr,
    ) -> Result<QueuePair> {
        let mut raw_attr = ibv_qp_init_attr {
            send_cq: send_cq.inner,
            recv_cq: recv_cq.inner,
            cap: ibv_qp_cap {
                max_send_wr: init_attr.max_send_wr,
                max_recv_wr: init_attr.max_recv_wr,
                max_send_sge: init_attr.max_send_sge,
                max_recv_sge: init_attr.max_recv_sge,
                max_inline_data: init_attr.max_inline_data,
            },
            qp_type: init_attr.qp_type.as_raw(),
            sq_sig_all: i32::from(init_attr.sq_sig_all),
            ..Default::default()
        };
        let qp = from_ptr(unsafe { ibv_create_qp(self.inner, &mut raw_attr) })?;
        Ok(QueuePair {
            inner: qp,
            _pd: Arc::clone(self),
            _send_cq: Arc::clone(send_cq),
            _recv_cq: Arc::clone(recv_cq),
        })
    }

    /// Raw pointer (for advanced/FFI use).
    pub fn as_raw(&self) -> *mut ibv_pd {
        self.inner
    }

    /// The parent context.
    pub fn context(&self) -> &Arc<Context> {
        &self.ctx
    }
}

impl Context {
    /// Allocate a Protection Domain.
    pub fn alloc_pd(self: &Arc<Self>) -> Result<Arc<ProtectionDomain>> {
        ProtectionDomain::new(Arc::clone(self))
    }
}
