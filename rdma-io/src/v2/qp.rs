//! V2 queue pair builder and typed operation methods.
//!
//! Provides [`QpBuilder`] for fluent QP configuration with documented defaults,
//! and [`Qp`] with typed methods for RDMA send, receive, read, and write
//! operations.

use std::sync::Arc;

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::*;

use crate::cm::{CmId, CmQueuePair};
use crate::error::from_ret;
use crate::qp::QpInitAttr;
use crate::wr::{RecvWr, SendWr, Sge, WrOpcode};

use super::cq::Cq;
use super::error::{Error, Result};
use super::mr::{Mr, RemoteMr};
use super::pd::Pd;

use crate::wc::WorkCompletion;

/// Builder for creating queue pairs with documented defaults.
///
/// # Defaults
///
/// | Parameter | Default |
/// |-----------|---------|
/// | QP type | RC (Reliable Connected) |
/// | max_send_wr | 16 |
/// | max_recv_wr | 16 |
/// | max_send_sge | 1 |
/// | max_recv_sge | 1 |
/// | sq_sig_all | true |
///
/// # Example
///
/// ```no_run
/// # use rdma_io::v2::*;
/// # fn example(pd: &Pd, send_cq: &Cq, recv_cq: &Cq, cm_id: &rdma_io::cm::CmId) -> Result<()> {
/// let qp = QpBuilder::new(pd, send_cq, recv_cq)
///     .max_send_wr(32)
///     .max_recv_wr(32)
///     .build_with_cm(cm_id)?;
/// # Ok(())
/// # }
/// ```
pub struct QpBuilder<'a> {
    pd: &'a Pd,
    send_cq: &'a Cq,
    recv_cq: &'a Cq,
    attr: QpInitAttr,
}

impl<'a> QpBuilder<'a> {
    /// Create a new QP builder with required parameters.
    ///
    /// The builder starts with documented defaults (see struct-level docs).
    /// Use the fluent methods to override specific parameters.
    pub fn new(pd: &'a Pd, send_cq: &'a Cq, recv_cq: &'a Cq) -> Self {
        Self {
            pd,
            send_cq,
            recv_cq,
            attr: QpInitAttr::default(),
        }
    }

    /// Set the maximum number of outstanding send work requests.
    ///
    /// Default: 16.
    pub fn max_send_wr(mut self, n: u32) -> Self {
        self.attr.max_send_wr = n;
        self
    }

    /// Set the maximum number of outstanding receive work requests.
    ///
    /// Default: 16.
    pub fn max_recv_wr(mut self, n: u32) -> Self {
        self.attr.max_recv_wr = n;
        self
    }

    /// Set the maximum scatter-gather entries per send WR.
    ///
    /// Default: 1.
    pub fn max_send_sge(mut self, n: u32) -> Self {
        self.attr.max_send_sge = n;
        self
    }

    /// Set the maximum scatter-gather entries per recv WR.
    ///
    /// Default: 1.
    pub fn max_recv_sge(mut self, n: u32) -> Self {
        self.attr.max_recv_sge = n;
        self
    }

    /// Set whether all send WRs generate completions.
    ///
    /// Default: true.
    pub fn sq_sig_all(mut self, enable: bool) -> Self {
        self.attr.sq_sig_all = enable;
        self
    }

    /// Build the QP using RDMA CM for connection management.
    ///
    /// The typical v2 flow is:
    /// 1. Establish CM connection (resolve address and route)
    /// 2. `Context::from_cm(&cm_id)` to get the device context
    /// 3. Allocate PD and CQs from that context
    /// 4. Call this method to create the QP
    ///
    /// The resulting [`Qp`] must be dropped **before** the `CmId` that
    /// created it. Use struct field ordering to ensure correct drop order.
    ///
    /// # Errors
    ///
    /// - [`Error::Verbs`] if QP creation fails
    pub fn build_with_cm(self, cm_id: &CmId) -> Result<Qp> {
        let pd_arc = Arc::clone(self.pd.inner());
        let send_cq_inner = Arc::clone(self.send_cq.inner());
        let recv_cq_inner = Arc::clone(self.recv_cq.inner());

        let cmqp = cm_id.create_qp_with_cq(
            &pd_arc,
            &self.attr,
            Some(&send_cq_inner),
            Some(&recv_cq_inner),
        )?;

        Ok(Qp { inner: cmqp })
    }

    /// Get the current QP initialization attributes.
    ///
    /// Useful for inspecting defaults or debugging.
    pub fn attr(&self) -> &QpInitAttr {
        &self.attr
    }
}

/// An RDMA queue pair with typed operation methods.
///
/// Created via [`QpBuilder::build_with_cm()`]. Provides ergonomic methods
/// for posting RDMA operations without manual WR construction.
///
/// # Drop Order
///
/// The `Qp` must be dropped **before** the `CmId` or `AsyncCmId` that
/// created it. When these are fields of the same struct, declare `Qp`
/// before the CM handle to ensure correct destruction order:
///
/// ```no_run
/// # use rdma_io::v2::*;
/// struct Connection {
///     qp: Qp,        // dropped first
///     // cm: AsyncCmId, // dropped second
/// }
/// ```
///
/// # Thread Safety
///
/// `Qp` is `Send + Sync`. The underlying QP uses internal locking
/// in libibverbs. However, callers must coordinate work request
/// submission and completion draining to avoid logical races.
pub struct Qp {
    inner: CmQueuePair,
}

impl Qp {
    /// Create from an existing `CmQueuePair`.
    ///
    /// Useful for interop when the QP was created through the v1 API
    /// (e.g., via `AsyncCmId::create_qp_with_cq()`).
    pub fn from_cm_qp(cmqp: CmQueuePair) -> Self {
        Self { inner: cmqp }
    }

    /// Post a send work request.
    ///
    /// Sends data from `mr` with the given `wr_id`. The `wr_id` is
    /// returned in the completion entry for correlation.
    ///
    /// # Errors
    ///
    /// - [`Error::PostFailed`] if the WR cannot be posted (e.g., QP in error state)
    pub fn post_send(&self, mr: &Mr, wr_id: u64) -> Result<()> {
        let sge = Sge::new(mr.addr(), mr.len() as u32, mr.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::Send).sg(sge);
        self.post_send_wr(&mut wr)
    }

    /// Post a receive work request.
    ///
    /// Prepares `mr` to receive incoming data. The `wr_id` is returned
    /// in the completion entry for correlation.
    ///
    /// # Errors
    ///
    /// - [`Error::PostFailed`] if the WR cannot be posted
    pub fn post_recv(&self, mr: &mut Mr, wr_id: u64) -> Result<()> {
        let sge = Sge::new(mr.addr(), mr.len() as u32, mr.lkey());
        let mut wr = RecvWr::new(wr_id).sg(sge);
        self.post_recv_wr(&mut wr)
    }

    /// Post an RDMA Write operation.
    ///
    /// Writes data from `local` to the remote memory described by `remote`.
    /// This is a one-sided operation — no receive is posted on the remote side.
    ///
    /// # Errors
    ///
    /// - [`Error::PostFailed`] if the WR cannot be posted
    pub fn post_write(
        &self,
        local: &Mr,
        remote: &RemoteMr,
        wr_id: u64,
    ) -> Result<()> {
        let sge = Sge::new(local.addr(), local.len() as u32, local.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::RdmaWrite)
            .sg(sge)
            .rdma(remote.addr, remote.rkey);
        self.post_send_wr(&mut wr)
    }

    /// Post an RDMA Read operation.
    ///
    /// Reads data from the remote memory described by `remote` into `local`.
    /// This is a one-sided operation — no send is posted on the remote side.
    ///
    /// # Errors
    ///
    /// - [`Error::PostFailed`] if the WR cannot be posted
    pub fn post_read(
        &self,
        local: &mut Mr,
        remote: &RemoteMr,
        wr_id: u64,
    ) -> Result<()> {
        let sge = Sge::new(local.addr(), local.len() as u32, local.lkey());
        let mut wr = SendWr::new(wr_id, WrOpcode::RdmaRead)
            .sg(sge)
            .rdma(remote.addr, remote.rkey);
        self.post_send_wr(&mut wr)
    }

    /// QP number assigned by the HCA.
    pub fn qp_num(&self) -> u32 {
        self.inner.qp_num()
    }

    /// Transition the QP to error state for teardown.
    ///
    /// Forces all outstanding WRs to complete (with flush error),
    /// enabling clean shutdown.
    pub fn to_error(&self) -> Result<()> {
        self.inner.to_error()?;
        Ok(())
    }

    /// Access the underlying CM queue pair for v1 API interop.
    pub fn inner(&self) -> &CmQueuePair {
        &self.inner
    }

    /// Submit a typed RDMA operation.
    ///
    /// io_uring/compio-style submission: pass a typed [`Op`] describing
    /// what to do, and the operation is posted to the hardware queue.
    /// The completion will carry the `wr_id` from the [`Op`] for correlation.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use rdma_io::v2::*;
    /// # fn example(qp: &Qp, mr: &Mr) -> Result<()> {
    /// qp.submit(Op::send(mr, 42))?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn submit(&self, op: super::op::Op<'_>) -> Result<()> {
        match op {
            super::op::Op::Send { mr, wr_id } => self.post_send(mr, wr_id),
            super::op::Op::Recv { mr, wr_id } => self.post_recv(mr, wr_id),
            super::op::Op::Write {
                local,
                remote,
                wr_id,
            } => self.post_write(local, remote, wr_id),
            super::op::Op::Read {
                local,
                remote,
                wr_id,
            } => self.post_read(local, remote, wr_id),
        }
    }

    /// Post a send and return an error if the completion indicates failure.
    ///
    /// Higher-level wrapper that checks the `WorkCompletion` status and
    /// converts failures to [`Error::CompletionError`].
    pub fn check_completion(wc: &WorkCompletion) -> Result<()> {
        if wc.is_success() {
            Ok(())
        } else {
            Err(Error::CompletionError {
                status: wc.status(),
                vendor_err: wc.vendor_err(),
            })
        }
    }

    // -- Internal posting helpers --

    fn post_send_wr(&self, wr: &mut SendWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        from_ret(unsafe {
            rdma_wrap_ibv_post_send(self.inner.as_raw(), &mut raw, &mut bad_wr)
        })
        .map_err(|e| match e {
            crate::Error::Verbs(io_err) => Error::PostFailed(io_err),
            other => Error::from(other),
        })
    }

    fn post_recv_wr(&self, wr: &mut RecvWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_recv_wr = std::ptr::null_mut();
        from_ret(unsafe {
            rdma_wrap_ibv_post_recv(self.inner.as_raw(), &mut raw, &mut bad_wr)
        })
        .map_err(|e| match e {
            crate::Error::Verbs(io_err) => Error::PostFailed(io_err),
            other => Error::from(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wr::QpType;

    #[test]
    fn test_builder_defaults() {
        // QpBuilder requires actual RDMA resources, but we can test
        // that the attribute defaults are correct by checking QpInitAttr.
        let attr = QpInitAttr::default();
        assert_eq!(attr.max_send_wr, 16);
        assert_eq!(attr.max_recv_wr, 16);
        assert_eq!(attr.max_send_sge, 1);
        assert_eq!(attr.max_recv_sge, 1);
        assert!(attr.sq_sig_all);
        assert_eq!(attr.qp_type, QpType::Rc);
    }
}
