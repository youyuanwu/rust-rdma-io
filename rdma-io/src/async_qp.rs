//! Async Queue Pair — mid-level async wrapper for RDMA verbs.
//!
//! `AsyncQp` wraps a raw `ibv_qp` pointer + `AsyncCq` and provides async
//! methods for individual RDMA verbs. Unlike `AsyncRdmaStream` (which
//! provides a buffered stream abstraction), `AsyncQp` exposes each verb
//! individually for full control.
//!
//! When using rdma_cm, the CM ID owns the QP — so `AsyncQp` borrows
//! the raw pointer rather than taking ownership of a `QueuePair`.

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::*;

use std::sync::Arc;

use crate::Result;
use crate::async_cq::AsyncCq;
use crate::cq::CompletionQueue;
use crate::error::from_ret;
use crate::mr::OwnedMemoryRegion;
use crate::wc::WorkCompletion;
use crate::wr::{RecvWr, SendFlags, SendWr, Sge, WrOpcode};

/// Async wrapper around a raw QP pointer for individual RDMA verb operations.
///
/// # Safety
/// The caller must ensure the `ibv_qp` pointer remains valid for the
/// lifetime of this struct (typically by keeping the owning `CmId` alive).
pub struct AsyncQp {
    qp: *mut ibv_qp,
    async_cq: AsyncCq,
}

// Safety: ibv_qp is thread-safe (protected by internal locking in libibverbs).
unsafe impl Send for AsyncQp {}
unsafe impl Sync for AsyncQp {}

impl AsyncQp {
    /// Create a new `AsyncQp` from a raw QP pointer and AsyncCq.
    ///
    /// # Safety
    /// The `ibv_qp` pointer must remain valid for the lifetime of this struct.
    pub unsafe fn new(qp: *mut ibv_qp, async_cq: AsyncCq) -> Self {
        Self { qp, async_cq }
    }

    /// Access the raw QP pointer.
    pub fn as_raw(&self) -> *mut ibv_qp {
        self.qp
    }

    /// Access the underlying AsyncCq.
    pub fn async_cq(&self) -> &AsyncCq {
        &self.async_cq
    }

    /// Access the underlying CQ.
    pub fn cq(&self) -> &Arc<CompletionQueue> {
        self.async_cq.cq()
    }

    fn post_send_raw(&self, wr: &mut SendWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_send(self.qp, &mut raw, &mut bad_wr) })
    }

    fn post_recv_raw(&self, wr: &mut RecvWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_recv_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_recv(self.qp, &mut raw, &mut bad_wr) })
    }

    /// Post a SEND and await its completion.
    ///
    /// Posts `length` bytes from `mr` starting at `offset`, then awaits
    /// the send completion identified by `wr_id`.
    pub async fn send(
        &self,
        mr: &OwnedMemoryRegion,
        offset: usize,
        length: usize,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(
            unsafe { (*mr.as_raw()).addr as u64 } + offset as u64,
            length as u32,
            mr.lkey(),
        );
        let mut wr = SendWr::new(wr_id, WrOpcode::Send)
            .flags(SendFlags::SIGNALED)
            .sg(sge);
        self.post_send_raw(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }

    /// Post a RECV buffer and await its completion.
    ///
    /// Returns the `WorkCompletion` which contains `byte_len()` for the
    /// received size.
    pub async fn recv(
        &self,
        mr: &OwnedMemoryRegion,
        offset: usize,
        length: usize,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(
            unsafe { (*mr.as_raw()).addr as u64 } + offset as u64,
            length as u32,
            mr.lkey(),
        );
        let mut wr = RecvWr::new(wr_id).sg(sge);
        self.post_recv_raw(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }

    /// Post a SEND with immediate data and await completion.
    pub async fn send_with_imm(
        &self,
        mr: &OwnedMemoryRegion,
        offset: usize,
        length: usize,
        imm_data: u32,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(
            unsafe { (*mr.as_raw()).addr as u64 } + offset as u64,
            length as u32,
            mr.lkey(),
        );
        let mut wr = SendWr::new(wr_id, WrOpcode::SendWithImm(imm_data))
            .flags(SendFlags::SIGNALED)
            .sg(sge);
        self.post_send_raw(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }

    /// Poll for completions directly (low-level access).
    pub async fn poll(&self, wc_buf: &mut [WorkCompletion]) -> Result<usize> {
        self.async_cq.poll(wc_buf).await
    }
}
