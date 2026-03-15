//! Async Queue Pair — mid-level async wrapper for RDMA verbs.
//!
//! `AsyncQp` wraps a [`CmQueuePair`] + separate send/recv [`AsyncCq`]s and
//! provides both async and poll-based methods for RDMA verbs.
//!
//! Unlike `AsyncRdmaStream` (which provides a buffered stream abstraction),
//! `AsyncQp` exposes each verb individually for full control.
//!
//! # Dual CQ Architecture
//!
//! Send and recv completions are routed to **separate CQs** to avoid
//! silent completion loss when both operations are in-flight concurrently.
//! See `docs/design/AsyncQpPolling.md` for the rationale.
//!
//! # Drop Order
//!
//! Field order matters: `qp` is declared before `send_cq`/`recv_cq`,
//! so Rust drops the QP before the CQs — matching the kernel-enforced
//! teardown order.

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::*;

use std::task::{Context, Poll};

use crate::Result;
use crate::async_cq::{AsyncCq, CqPollState};
use crate::cm::CmQueuePair;
use crate::error::from_ret;
use crate::mr::{OwnedMemoryRegion, RemoteMr};
use crate::wc::WorkCompletion;
use crate::wr::{RecvWr, SendFlags, SendWr, Sge, WrOpcode};

/// Async wrapper owning a CM-managed QP and its async completion queues.
///
/// Drop order: `qp` first (destroys QP), then `send_cq`, then `recv_cq`.
pub struct AsyncQp {
    // IMPORTANT: field order = drop order. QP must die before CQs.
    qp: CmQueuePair,
    send_cq: AsyncCq,
    recv_cq: AsyncCq,
}

impl AsyncQp {
    /// Create a new `AsyncQp` with separate send and recv CQs.
    pub fn new(qp: CmQueuePair, send_cq: AsyncCq, recv_cq: AsyncCq) -> Self {
        Self {
            qp,
            send_cq,
            recv_cq,
        }
    }

    /// Access the raw QP pointer.
    pub fn as_raw(&self) -> *mut ibv_qp {
        self.qp.as_raw()
    }

    /// Access the send completion queue (for CQ drain in teardown).
    pub fn send_cq(&self) -> &AsyncCq {
        &self.send_cq
    }

    /// Access the recv completion queue (for CQ drain in teardown).
    pub fn recv_cq(&self) -> &AsyncCq {
        &self.recv_cq
    }

    // --- Post helpers (pub(crate) for use by ring transport) ---

    /// Post an arbitrary send WR to the QP.
    pub(crate) fn post_send_wr(&self, wr: &mut SendWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_send(self.qp.as_raw(), &mut raw, &mut bad_wr) })
    }

    /// Post an arbitrary recv WR to the QP.
    pub(crate) fn post_recv_wr(&self, wr: &mut RecvWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_recv_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_recv(self.qp.as_raw(), &mut raw, &mut bad_wr) })
    }

    // --- Fire-and-forget post methods ---

    /// Post a SEND without waiting for completion.
    ///
    /// Used by poll-based `AsyncWrite` which separates post from completion polling.
    pub fn post_send_signaled(
        &self,
        mr: &OwnedMemoryRegion,
        offset: usize,
        length: usize,
        wr_id: u64,
    ) -> Result<()> {
        let sge = Sge::new(
            unsafe { (*mr.as_raw()).addr as u64 } + offset as u64,
            length as u32,
            mr.lkey(),
        );
        let mut wr = SendWr::new(wr_id, WrOpcode::Send)
            .flags(SendFlags::SIGNALED)
            .sg(sge);
        self.post_send_wr(&mut wr)
    }

    /// Post a RECV buffer without waiting for completion.
    ///
    /// Used by `AsyncRdmaStream` to pre-post recv buffers and re-post
    /// after consuming a recv completion.
    pub fn post_recv_buffer(&self, mr: &OwnedMemoryRegion, wr_id: u64) -> Result<()> {
        let sge = Sge::new(
            unsafe { (*mr.as_raw()).addr as u64 },
            mr.as_slice().len() as u32,
            mr.lkey(),
        );
        let mut wr = RecvWr::new(wr_id).sg(sge);
        self.post_recv_wr(&mut wr)
    }

    // --- Poll-based CQ accessors (for AsyncRead/AsyncWrite impls) ---

    /// Poll the send CQ for completions.
    #[inline]
    pub fn poll_send_cq(
        &self,
        cx: &mut Context<'_>,
        state: &mut CqPollState,
        wc_buf: &mut [WorkCompletion],
    ) -> Poll<Result<usize>> {
        self.send_cq.poll_completions(cx, state, wc_buf)
    }

    /// Poll the recv CQ for completions.
    #[inline]
    pub fn poll_recv_cq(
        &self,
        cx: &mut Context<'_>,
        state: &mut CqPollState,
        wc_buf: &mut [WorkCompletion],
    ) -> Poll<Result<usize>> {
        self.recv_cq.poll_completions(cx, state, wc_buf)
    }

    // --- Async convenience methods ---

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
        self.post_send_wr(&mut wr)?;
        self.send_cq.poll_wr_id(wr_id).await
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
        self.post_recv_wr(&mut wr)?;
        self.recv_cq.poll_wr_id(wr_id).await
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
        self.post_send_wr(&mut wr)?;
        self.send_cq.poll_wr_id(wr_id).await
    }

    // --- One-sided RDMA verbs ---

    /// RDMA READ: read data from a remote memory region into a local buffer.
    ///
    /// The remote side is not notified. The local `mr` receives the data.
    pub async fn read_remote(
        &self,
        mr: &OwnedMemoryRegion,
        local_offset: usize,
        length: usize,
        remote: &RemoteMr,
        remote_offset: u64,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(mr.addr() + local_offset as u64, length as u32, mr.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::RdmaRead)
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .rdma(remote.addr + remote_offset, remote.rkey);
        self.post_send_wr(&mut wr)?;
        self.send_cq.poll_wr_id(wr_id).await
    }

    /// RDMA WRITE: write data from a local buffer to a remote memory region.
    ///
    /// The remote side is not notified (no completion on remote CQ).
    pub async fn write_remote(
        &self,
        mr: &OwnedMemoryRegion,
        local_offset: usize,
        length: usize,
        remote: &RemoteMr,
        remote_offset: u64,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(mr.addr() + local_offset as u64, length as u32, mr.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::RdmaWrite)
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .rdma(remote.addr + remote_offset, remote.rkey);
        self.post_send_wr(&mut wr)?;
        self.send_cq.poll_wr_id(wr_id).await
    }

    /// RDMA WRITE with immediate data.
    ///
    /// Like `write_remote`, but the immediate data generates a recv completion
    /// on the remote side (the remote must have a posted recv WR).
    #[allow(clippy::too_many_arguments)]
    pub async fn write_remote_with_imm(
        &self,
        mr: &OwnedMemoryRegion,
        local_offset: usize,
        length: usize,
        remote: &RemoteMr,
        remote_offset: u64,
        imm_data: u32,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(mr.addr() + local_offset as u64, length as u32, mr.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::RdmaWriteWithImm(imm_data))
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .rdma(remote.addr + remote_offset, remote.rkey);
        self.post_send_wr(&mut wr)?;
        self.send_cq.poll_wr_id(wr_id).await
    }

    // --- Atomic verbs ---

    /// Atomic Compare-and-Swap on a remote 8-byte value.
    ///
    /// Atomically: if `*remote == compare`, set `*remote = swap`.
    /// The original remote value is written to `result_mr` at `result_offset`.
    /// The result buffer must be at least 8 bytes at the given offset.
    #[allow(clippy::too_many_arguments)]
    pub async fn compare_and_swap(
        &self,
        result_mr: &OwnedMemoryRegion,
        result_offset: usize,
        remote: &RemoteMr,
        remote_offset: u64,
        compare: u64,
        swap: u64,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(result_mr.addr() + result_offset as u64, 8, result_mr.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::AtomicCmpAndSwp)
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .atomic(remote.addr + remote_offset, remote.rkey, compare, swap);
        self.post_send_wr(&mut wr)?;
        self.send_cq.poll_wr_id(wr_id).await
    }

    /// Atomic Fetch-and-Add on a remote 8-byte value.
    ///
    /// Atomically: `old = *remote; *remote += add_value; return old`.
    /// The original remote value is written to `result_mr` at `result_offset`.
    /// The result buffer must be at least 8 bytes at the given offset.
    pub async fn fetch_and_add(
        &self,
        result_mr: &OwnedMemoryRegion,
        result_offset: usize,
        remote: &RemoteMr,
        remote_offset: u64,
        add_value: u64,
        wr_id: u64,
    ) -> Result<WorkCompletion> {
        let sge = Sge::new(result_mr.addr() + result_offset as u64, 8, result_mr.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::AtomicFetchAndAdd)
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .atomic(remote.addr + remote_offset, remote.rkey, add_value, 0);
        self.post_send_wr(&mut wr)?;
        self.send_cq.poll_wr_id(wr_id).await
    }
}
