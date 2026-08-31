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
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

use super::cq::Cq;
use super::error::{Error, Result};
use super::mr::{Mr, RemoteMr};
use super::pd::Pd;

use crate::wc::WorkCompletion;

/// Provider result for one linked work-request batch.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "prefix fields are consumed by the Tokio-gated engine"
)]
pub(crate) enum BatchPostOutcome {
    AllAccepted,
    PrefixAccepted {
        accepted: usize,
        first_unaccepted: usize,
        source: std::io::Error,
    },
    Ambiguous {
        source: std::io::Error,
    },
}

/// QP capacities returned by the provider after creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "consumed by Phase 3 test hooks and Phase 4 CM installation"
)]
pub(crate) struct QpCapabilities {
    pub(crate) max_send_wr: u32,
    pub(crate) max_recv_wr: u32,
    pub(crate) max_send_sge: u32,
    pub(crate) max_recv_sge: u32,
}

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
    /// - [`Error::InvalidConfig`] if WR or SGE capacities are zero
    /// - [`Error::Verbs`] if QP creation fails
    pub fn build_with_cm(self, cm_id: &CmId) -> Result<Qp> {
        // Validate builder parameters
        if self.attr.max_send_wr == 0 {
            return Err(Error::InvalidConfig("max_send_wr must be > 0".into()));
        }
        if self.attr.max_recv_wr == 0 {
            return Err(Error::InvalidConfig("max_recv_wr must be > 0".into()));
        }
        if self.attr.max_send_sge == 0 {
            return Err(Error::InvalidConfig("max_send_sge must be > 0".into()));
        }
        if self.attr.max_recv_sge == 0 {
            return Err(Error::InvalidConfig("max_recv_sge must be > 0".into()));
        }

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
        let wr = SendWr::new(wr_id, WrOpcode::Send)
            .sg(sge)
            .flags(SendFlags::SIGNALED);
        self.post_single_send(wr)
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
        let wr = RecvWr::new(wr_id).sg(sge);
        self.post_single_recv(wr)
    }

    /// Post an RDMA Write operation.
    ///
    /// Writes data from `local` to the remote memory described by `remote`.
    /// This is a one-sided operation — no receive is posted on the remote side.
    ///
    /// # Errors
    ///
    /// - [`Error::PostFailed`] if the WR cannot be posted
    pub fn post_write(&self, local: &Mr, remote: &RemoteMr, wr_id: u64) -> Result<()> {
        let sge = Sge::new(local.addr(), local.len() as u32, local.lkey());
        let wr = SendWr::new(wr_id, WrOpcode::RdmaWrite)
            .sg(sge)
            .rdma(remote.addr, remote.rkey)
            .flags(SendFlags::SIGNALED);
        self.post_single_send(wr)
    }

    /// Post an RDMA Read operation.
    ///
    /// Reads data from the remote memory described by `remote` into `local`.
    /// This is a one-sided operation — no send is posted on the remote side.
    ///
    /// # Errors
    ///
    /// - [`Error::PostFailed`] if the WR cannot be posted
    pub fn post_read(&self, local: &mut Mr, remote: &RemoteMr, wr_id: u64) -> Result<()> {
        let sge = Sge::new(local.addr(), local.len() as u32, local.lkey());
        let wr = SendWr::new(wr_id, WrOpcode::RdmaRead)
            .sg(sge)
            .rdma(remote.addr, remote.rkey)
            .flags(SendFlags::SIGNALED);
        self.post_single_send(wr)
    }

    /// QP number assigned by the HCA.
    pub fn qp_num(&self) -> u32 {
        self.inner.qp_num()
    }

    #[allow(
        dead_code,
        reason = "consumed by Phase 3 test hooks and Phase 4 CM installation"
    )]
    pub(crate) fn capabilities(&self) -> QpCapabilities {
        let capabilities = self.inner.capabilities();
        QpCapabilities {
            max_send_wr: capabilities.max_send_wr,
            max_recv_wr: capabilities.max_recv_wr,
            max_send_sge: capabilities.max_send_sge,
            max_recv_sge: capabilities.max_recv_sge,
        }
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

    pub(crate) fn destroy(self) {
        self.inner.destroy();
    }

    #[allow(
        dead_code,
        reason = "consumed by engine test hooks and Phase 4 CM installation"
    )]
    pub(crate) fn uses_resources(&self, pd: &Pd, cq: &Cq) -> bool {
        self.inner
            .uses_resources(pd.inner(), cq.inner(), cq.inner())
    }

    /// Submit a typed RDMA operation.
    ///
    /// io_uring/compio-style submission: pass a typed [`Op`](super::op::Op)
    /// describing what to do, and the operation is posted to the hardware
    /// queue. The completion will carry the `wr_id` from the
    /// [`Op`](super::op::Op) for correlation.
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

    fn post_single_send(&self, wr: SendWr) -> Result<()> {
        let mut batch = PreparedSendBatch::new(vec![wr]).map_err(Error::from)?;
        batch_outcome_to_single(self.post_send_batch(&mut batch))
    }

    fn post_single_recv(&self, wr: RecvWr) -> Result<()> {
        let mut batch = PreparedRecvBatch::new(vec![wr]).map_err(Error::from)?;
        batch_outcome_to_single(self.post_recv_batch(&mut batch))
    }

    fn post_send_wr(&self, wr: &mut SendWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_send(self.inner.as_raw(), &mut raw, &mut bad_wr) })
            .map_err(|e| match e {
                crate::Error::Verbs(io_err) => Error::PostFailed(io_err),
                other => Error::from(other),
            })
    }

    fn post_recv_wr(&self, wr: &mut RecvWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_recv_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_recv(self.inner.as_raw(), &mut raw, &mut bad_wr) })
            .map_err(|e| match e {
                crate::Error::Verbs(io_err) => Error::PostFailed(io_err),
                other => Error::from(other),
            })
    }

    /// Post a raw send WR. Used by the per-operation future infrastructure.
    pub(crate) fn post_send_wr_raw(&self, wr: &mut SendWr) -> Result<()> {
        self.post_send_wr(wr)
    }

    /// Post a raw recv WR. Used by the per-operation future infrastructure.
    pub(crate) fn post_recv_wr_raw(&self, wr: &mut RecvWr) -> Result<()> {
        self.post_recv_wr(wr)
    }

    pub(crate) fn post_send_batch(&self, batch: &mut PreparedSendBatch) -> BatchPostOutcome {
        debug_assert!(!batch.is_empty());
        let mut bad_wr = std::ptr::null_mut();
        let ret =
            unsafe { rdma_wrap_ibv_post_send(self.inner.as_raw(), batch.head_mut(), &mut bad_wr) };
        classify_send_post_result(batch, ret, bad_wr)
    }

    pub(crate) fn post_recv_batch(&self, batch: &mut PreparedRecvBatch) -> BatchPostOutcome {
        debug_assert!(!batch.is_empty());
        let mut bad_wr = std::ptr::null_mut();
        let ret =
            unsafe { rdma_wrap_ibv_post_recv(self.inner.as_raw(), batch.head_mut(), &mut bad_wr) };
        classify_recv_post_result(batch, ret, bad_wr)
    }
}

fn post_error(ret: i32) -> std::io::Error {
    match crate::error::from_ret(ret) {
        Err(crate::Error::Verbs(error)) => error,
        Err(error) => std::io::Error::other(error.to_string()),
        Ok(()) => std::io::Error::other("verbs post unexpectedly reported success"),
    }
}

fn classify_send_post_result(
    batch: &PreparedSendBatch,
    ret: i32,
    bad_wr: *mut ibv_send_wr,
) -> BatchPostOutcome {
    if ret == 0 {
        return BatchPostOutcome::AllAccepted;
    }
    let source = post_error(ret);
    match batch.first_unaccepted(bad_wr) {
        Some(first_unaccepted)
            if first_unaccepted < batch.len()
                && batch.ledger_index(first_unaccepted) == Some(first_unaccepted) =>
        {
            BatchPostOutcome::PrefixAccepted {
                accepted: first_unaccepted,
                first_unaccepted,
                source,
            }
        }
        None => BatchPostOutcome::Ambiguous { source },
        Some(_) => BatchPostOutcome::Ambiguous { source },
    }
}

fn classify_recv_post_result(
    batch: &PreparedRecvBatch,
    ret: i32,
    bad_wr: *mut ibv_recv_wr,
) -> BatchPostOutcome {
    if ret == 0 {
        return BatchPostOutcome::AllAccepted;
    }
    let source = post_error(ret);
    match batch.first_unaccepted(bad_wr) {
        Some(first_unaccepted)
            if first_unaccepted < batch.len()
                && batch.ledger_index(first_unaccepted) == Some(first_unaccepted) =>
        {
            BatchPostOutcome::PrefixAccepted {
                accepted: first_unaccepted,
                first_unaccepted,
                source,
            }
        }
        None => BatchPostOutcome::Ambiguous { source },
        Some(_) => BatchPostOutcome::Ambiguous { source },
    }
}

fn batch_outcome_to_single(outcome: BatchPostOutcome) -> Result<()> {
    match outcome {
        BatchPostOutcome::AllAccepted => Ok(()),
        BatchPostOutcome::PrefixAccepted { source, .. }
        | BatchPostOutcome::Ambiguous { source } => Err(Error::PostFailed(source)),
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

    #[test]
    fn bad_wr_classifies_every_send_prefix_and_ambiguity() {
        for first_unaccepted in 0..4 {
            let mut batch = PreparedSendBatch::new(
                (0..4)
                    .map(|index| SendWr::new(index, WrOpcode::Send))
                    .collect(),
            )
            .unwrap();
            let bad_wr = batch.member_ptr_for_test(first_unaccepted);
            assert!(matches!(
                classify_send_post_result(&batch, -libc::ENOMEM, bad_wr),
                BatchPostOutcome::PrefixAccepted {
                    accepted,
                    first_unaccepted: first,
                    ..
                } if accepted == first_unaccepted && first == first_unaccepted
            ));
        }

        let batch = PreparedSendBatch::new(vec![SendWr::new(1, WrOpcode::Send)]).unwrap();
        assert!(matches!(
            classify_send_post_result(&batch, -libc::ENOMEM, std::ptr::null_mut()),
            BatchPostOutcome::Ambiguous { .. }
        ));
    }

    #[test]
    fn bad_wr_classifies_every_recv_prefix_and_ambiguity() {
        let mut batch = PreparedRecvBatch::new((0..3).map(RecvWr::new).collect()).unwrap();
        let bad_wr = batch.member_ptr_for_test(2);
        assert!(matches!(
            classify_recv_post_result(&batch, -libc::ENOMEM, bad_wr),
            BatchPostOutcome::PrefixAccepted {
                accepted: 2,
                first_unaccepted: 2,
                ..
            }
        ));
        assert!(matches!(
            classify_recv_post_result(&batch, -libc::ENOMEM, std::ptr::null_mut()),
            BatchPostOutcome::Ambiguous { .. }
        ));
    }
}
