//! Per-operation async futures and `SharedQp` for compio/tokio-uring-style usage.
//!
//! Provides [`SharedQp`], a queue pair handle with per-operation futures that
//! submit an RDMA operation and await its individual completion. Each operation
//! retains owned resources (compio-style buffer ownership) until the CQE arrives,
//! guaranteeing memory safety even if the future is dropped mid-flight.
//!
//! # Design
//!
//! Follows compio/tokio-uring's per-operation future pattern:
//! - Each async method takes **owned** buffers (`Mr`) and returns them with
//!   the completion result
//! - A shared [`InflightMap`] routes CQEs to individual futures by `wr_id` token
//! - A completion driver task (spawned separately) drains the CQ and delivers
//!   completions
//! - Dropping a future does NOT cancel the RDMA operation — the owned resources
//!   are moved into detached state and reclaimed when the CQE eventually arrives
//!
//! # Example
//!
//! ```no_run
//! # use rdma_io::v2::*;
//! # async fn example(sqp: &SharedQp) -> Result<()> {
//! let pd = sqp.pd();
//! let mut mr = pd.reg_mr(64, AccessIntent::LocalOnly)?;
//! mr.as_mut_slice().copy_from_slice(b"hello");
//!
//! // compio-style: owned buffer in, (result, buffer) out
//! let (result, mr) = sqp.send(mr, None).await;
//! result?;
//! // mr is returned and can be reused
//! # Ok(())
//! # }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::wr::{RecvWr, SendWr, Sge, WrOpcode};

use super::driver::CqDriverHandle;
use super::error::{Error, Result};
use super::mr::{Mr, RemoteMr};
use super::op::Completion;
use super::pd::Pd;
use super::qp::Qp;

/// A queue pair with per-operation async future support.
///
/// Wraps a [`Qp`] and a shared [`CqDriverHandle`] to provide
/// compio/tokio-uring-style operation futures. Each async method
/// submits an RDMA operation and returns a future that resolves
/// when the individual CQE arrives.
///
/// # Construction
///
/// Create via [`SharedQp::new()`] after setting up a [`Qp`] and
/// spawning a completion driver.
///
/// # Buffer Ownership
///
/// All operation methods take **owned** `Mr` values and return them
/// with the completion result. This guarantees the MR stays registered
/// and accessible while the HCA may be using it, even if the future
/// is dropped.
///
/// # Cancellation
///
/// Dropping an operation future does NOT cancel the RDMA operation
/// (RDMA has no general per-WR cancellation). The owned `Mr` is moved
/// to a detached lease that is reclaimed when the CQE eventually arrives.
pub struct SharedQp {
    qp: Arc<Qp>,
    /// Driver handle for send-side completions (send, write, read operations).
    send_handle: Arc<CqDriverHandle>,
    /// Driver handle for recv-side completions.
    recv_handle: Arc<CqDriverHandle>,
    pd: Pd,
}

impl SharedQp {
    /// Create a new `SharedQp` from a queue pair and separate send/recv driver handles.
    ///
    /// RDMA uses separate send and recv completion queues; each needs its own
    /// driver. Send/write/read completions route through `send_handle`, while
    /// recv completions route through `recv_handle`.
    ///
    /// The `pd` is stored for MR registration convenience.
    /// The `qp` is wrapped in `Arc` for shared access.
    pub fn new(
        qp: Qp,
        send_handle: Arc<CqDriverHandle>,
        recv_handle: Arc<CqDriverHandle>,
        pd: Pd,
    ) -> Self {
        Self {
            qp: Arc::new(qp),
            send_handle,
            recv_handle,
            pd,
        }
    }

    /// Access the protection domain for MR registration.
    pub fn pd(&self) -> &Pd {
        &self.pd
    }

    /// Access the underlying QP.
    pub fn qp(&self) -> &Arc<Qp> {
        &self.qp
    }

    /// Submit a send operation and await its completion.
    ///
    /// Returns `(Result<Completion>, Mr)` — the MR is always returned
    /// regardless of success or failure.
    ///
    /// `range` optionally specifies a byte sub-range `(offset, length)`
    /// within the MR to send. If `None`, the entire MR is sent.
    pub fn send(&self, mr: Mr, range: Option<(usize, usize)>) -> OpFuture {
        let (offset, len) = range.unwrap_or((0, mr.len()));
        OpFuture::new(
            Arc::clone(&self.qp),
            Arc::clone(&self.send_handle),
            OpKind::Send,
            mr,
            None,
            offset,
            len,
        )
    }

    /// Submit a receive operation and await its completion.
    ///
    /// Returns `(Result<Completion>, Mr)` — the MR is returned with
    /// the received data written into it.
    ///
    /// `range` optionally specifies a byte sub-range for the receive buffer.
    pub fn recv(&self, mr: Mr, range: Option<(usize, usize)>) -> OpFuture {
        let (offset, len) = range.unwrap_or((0, mr.len()));
        OpFuture::new(
            Arc::clone(&self.qp),
            Arc::clone(&self.recv_handle),
            OpKind::Recv,
            mr,
            None,
            offset,
            len,
        )
    }

    /// Submit an RDMA Write and await its completion.
    ///
    /// Writes data from `local` to the remote memory described by `remote`.
    ///
    /// `range` optionally specifies a byte sub-range of `local` to write.
    pub fn write(&self, local: Mr, remote: RemoteMr, range: Option<(usize, usize)>) -> OpFuture {
        let (offset, len) = range.unwrap_or((0, local.len()));
        OpFuture::new(
            Arc::clone(&self.qp),
            Arc::clone(&self.send_handle),
            OpKind::Write,
            local,
            Some(remote),
            offset,
            len,
        )
    }

    /// Submit an RDMA Read and await its completion.
    ///
    /// Reads data from the remote memory into `local`.
    ///
    /// `range` optionally specifies a byte sub-range of `local` to fill.
    pub fn read(&self, local: Mr, remote: RemoteMr, range: Option<(usize, usize)>) -> OpFuture {
        let (offset, len) = range.unwrap_or((0, local.len()));
        OpFuture::new(
            Arc::clone(&self.qp),
            Arc::clone(&self.send_handle),
            OpKind::Read,
            local,
            Some(remote),
            offset,
            len,
        )
    }

    /// Transition QP to error state, flushing all in-flight operations.
    ///
    /// This moves the QP to `IBV_QPS_ERR`, causing the HCA to flush all
    /// outstanding WRs. It does NOT stop the completion drivers — they
    /// continue running to drain the resulting flush CQEs.
    ///
    /// Use [`shutdown_drivers()`](Self::shutdown_drivers) when this `SharedQp`
    /// is the sole owner of its driver handles and the drivers should stop.
    pub fn shutdown(&self) -> Result<()> {
        self.qp.to_error()?;
        Ok(())
    }

    /// Signal both completion drivers to shut down.
    ///
    /// Only call this when this `SharedQp` is the sole owner of its driver
    /// handles. If the drivers are shared across multiple QPs, the driver
    /// owner should call `handle.shutdown()` directly.
    pub fn shutdown_drivers(&self) {
        self.send_handle.shutdown();
        self.recv_handle.shutdown();
    }
}

/// The kind of RDMA operation.
#[derive(Debug, Clone, Copy)]
enum OpKind {
    Send,
    Recv,
    Write,
    Read,
}

/// State of an operation future.
enum OpState {
    /// Not yet posted. Holds resources needed for posting.
    Pending {
        qp: Arc<Qp>,
        handle: Arc<CqDriverHandle>,
        kind: OpKind,
        mr: Mr,
        remote: Option<RemoteMr>,
        offset: usize,
        len: usize,
    },
    /// Posted and waiting for completion.
    Inflight {
        handle: Arc<CqDriverHandle>,
        token: u64,
        /// Owned MR kept alive while HCA accesses it.
        mr: Mr,
    },
    /// Completed or taken.
    Done,
}

/// A future representing a single in-flight RDMA operation.
///
/// Resolves to `(Result<Completion>, Mr)` — the owned MR is always
/// returned regardless of success or failure.
///
/// # Cancellation Safety
///
/// Dropping this future does NOT cancel the RDMA operation. The
/// owned `Mr` and registry slot are pushed to the driver's centralized
/// reclaim queue, which releases them when the CQE arrives. No
/// per-operation task is spawned.
pub struct OpFuture {
    state: OpState,
    /// Optional callback invoked when this future is dropped while in-flight
    /// and the completion eventually arrives. Used by higher layers (e.g.,
    /// message transport) to return the MR to a buffer pool.
    cancel_reclaim: Option<Box<dyn FnOnce(Mr) + Send>>,
}

impl OpFuture {
    fn new(
        qp: Arc<Qp>,
        handle: Arc<CqDriverHandle>,
        kind: OpKind,
        mr: Mr,
        remote: Option<RemoteMr>,
        offset: usize,
        len: usize,
    ) -> Self {
        Self {
            state: OpState::Pending {
                qp,
                handle,
                kind,
                mr,
                remote,
                offset,
                len,
            },
            cancel_reclaim: None,
        }
    }

    /// Attach a callback that will be invoked with the owned `Mr` if this
    /// future is dropped while in-flight and the CQE eventually arrives.
    ///
    /// This enables higher layers to return the MR to a buffer pool after
    /// cancellation without leaking it.
    pub fn on_cancel_reclaim(mut self, cb: Box<dyn FnOnce(Mr) + Send>) -> Self {
        self.cancel_reclaim = Some(cb);
        self
    }
}

impl Future for OpFuture {
    type Output = (Result<Completion>, Mr);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };

        loop {
            match &mut this.state {
                OpState::Pending { .. } => {
                    // Take ownership of the pending state
                    let pending = std::mem::replace(&mut this.state, OpState::Done);
                    let OpState::Pending {
                        qp,
                        handle,
                        kind,
                        mr,
                        remote,
                        offset,
                        len,
                    } = pending
                    else {
                        unreachable!()
                    };

                    // Register in the inflight map
                    let reg = match handle.map().register() {
                        Some(r) => r,
                        None => {
                            return Poll::Ready((Err(Error::CapacityExhausted), mr));
                        }
                    };

                    let token = reg.token;

                    // Post the WR with the token as wr_id
                    let post_result =
                        post_operation(&qp, kind, &mr, remote.as_ref(), token, offset, len);

                    if let Err(e) = post_result {
                        // Post failed — release registry slot and return MR
                        handle.map().release(token);
                        return Poll::Ready((Err(e), mr));
                    }

                    // Move to inflight state
                    this.state = OpState::Inflight { handle, token, mr };
                    // Fall through to poll the inflight state
                }
                OpState::Inflight { handle, token, .. } => {
                    // Register waker FIRST (register-check-recheck pattern)
                    handle.map().register_waker(*token, cx.waker());

                    // Check if completion arrived (catches race)
                    if let Some(wc) = handle.map().take_completion(*token) {
                        let token = *token;
                        let inflight = std::mem::replace(&mut this.state, OpState::Done);
                        let OpState::Inflight { handle, mr, .. } = inflight else {
                            unreachable!()
                        };
                        handle.map().release(token);

                        let completion = Completion::from(wc);
                        let result = completion.result().map(|()| completion);
                        return Poll::Ready((result, mr));
                    }

                    // No completion yet — waker already registered above
                    return Poll::Pending;
                }
                OpState::Done => {
                    panic!("OpFuture polled after completion");
                }
            }
        }
    }
}

impl Drop for OpFuture {
    fn drop(&mut self) {
        match std::mem::replace(&mut self.state, OpState::Done) {
            OpState::Inflight { handle, token, mr } => {
                // Operation was posted but future is being dropped.
                // The HCA may still be accessing the MR — push to
                // the driver's centralized reclaim queue instead of
                // spawning a per-operation task.
                let on_reclaim = self.cancel_reclaim.take();
                handle.push_detached(token, mr, on_reclaim);
            }
            OpState::Pending { .. } => {
                // Never posted — nothing to clean up.
            }
            OpState::Done => {}
        }
    }
}

/// Post an RDMA operation to the QP.
fn post_operation(
    qp: &Qp,
    kind: OpKind,
    mr: &Mr,
    remote: Option<&RemoteMr>,
    wr_id: u64,
    offset: usize,
    len: usize,
) -> Result<()> {
    use crate::wr::SendFlags;
    let addr = mr.addr() + offset as u64;
    let sge = Sge::new(addr, len as u32, mr.lkey());

    match kind {
        OpKind::Send => {
            let mut wr = SendWr::new(wr_id, WrOpcode::Send)
                .sg(sge)
                .flags(SendFlags::SIGNALED);
            qp.post_send_wr_raw(&mut wr)
        }
        OpKind::Recv => {
            let mut wr = RecvWr::new(wr_id).sg(sge);
            qp.post_recv_wr_raw(&mut wr)
        }
        OpKind::Write => {
            let r = remote.expect("Write requires remote");
            let mut wr = SendWr::new(wr_id, WrOpcode::RdmaWrite)
                .sg(sge)
                .rdma(r.addr, r.rkey)
                .flags(SendFlags::SIGNALED);
            qp.post_send_wr_raw(&mut wr)
        }
        OpKind::Read => {
            let r = remote.expect("Read requires remote");
            let mut wr = SendWr::new(wr_id, WrOpcode::RdmaRead)
                .sg(sge)
                .rdma(r.addr, r.rkey)
                .flags(SendFlags::SIGNALED);
            qp.post_send_wr_raw(&mut wr)
        }
    }
}
