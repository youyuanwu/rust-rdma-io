//! Queue Pair.

use std::sync::Arc;

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::*;

use crate::Result;
use crate::cq::CompletionQueue;
use crate::error::from_ret;
use crate::pd::ProtectionDomain;
use crate::wr::{QpState, QpType, RecvWr, SendWr};

/// Initialization attributes for a Queue Pair.
#[derive(Debug, Clone)]
pub struct QpInitAttr {
    /// QP type (RC, UC, UD, …).
    pub qp_type: QpType,
    /// Maximum outstanding send work requests.
    pub max_send_wr: u32,
    /// Maximum outstanding recv work requests.
    pub max_recv_wr: u32,
    /// Maximum scatter-gather entries per send WR.
    pub max_send_sge: u32,
    /// Maximum scatter-gather entries per recv WR.
    pub max_recv_sge: u32,
    /// Maximum inline data size.
    pub max_inline_data: u32,
    /// If true, all send WRs generate a completion.
    pub sq_sig_all: bool,
}

impl Default for QpInitAttr {
    fn default() -> Self {
        Self {
            qp_type: QpType::Rc,
            max_send_wr: 16,
            max_recv_wr: 16,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: 0,
            sq_sig_all: true,
        }
    }
}

/// An RDMA Queue Pair (`ibv_qp`).
pub struct QueuePair {
    pub(crate) inner: *mut ibv_qp,
    pub(crate) _pd: Arc<ProtectionDomain>,
    pub(crate) _send_cq: Arc<CompletionQueue>,
    pub(crate) _recv_cq: Arc<CompletionQueue>,
}

// Safety: ibv_qp is thread-safe (protected by internal locking in libibverbs).
unsafe impl Send for QueuePair {}
unsafe impl Sync for QueuePair {}

impl Drop for QueuePair {
    fn drop(&mut self) {
        let ret = unsafe { ibv_destroy_qp(self.inner) };
        if ret != 0 {
            tracing::error!(
                "ibv_destroy_qp failed: {}",
                std::io::Error::from_raw_os_error(-ret)
            );
        }
    }
}

impl QueuePair {
    /// QP number assigned by the HCA.
    pub fn qp_num(&self) -> u32 {
        unsafe { (*self.inner).qp_num }
    }

    /// Current QP state.
    pub fn state(&self) -> QpState {
        QpState::from_raw(unsafe { (*self.inner).state })
    }

    /// Modify the QP with the given attribute mask.
    ///
    /// This is the low-level modify call. For convenience, use
    /// [`to_init`](Self::to_init), [`to_rtr`](Self::to_rtr), [`to_rts`](Self::to_rts).
    pub fn modify(&self, attr: &mut ibv_qp_attr, attr_mask: u32) -> Result<()> {
        from_ret(unsafe { ibv_modify_qp(self.inner, attr, attr_mask as i32) })
    }

    /// Transition QP to INIT state.
    pub fn to_init(&self, port_num: u8, pkey_index: u16, access_flags: u32) -> Result<()> {
        let mut attr = ibv_qp_attr {
            qp_state: IBV_QPS_INIT,
            pkey_index,
            port_num,
            qp_access_flags: access_flags,
            ..Default::default()
        };
        let mask = IBV_QP_STATE | IBV_QP_PKEY_INDEX | IBV_QP_PORT | IBV_QP_ACCESS_FLAGS;
        self.modify(&mut attr, mask)
    }

    /// Transition QP to RTR state (for RC QPs).
    ///
    /// `dest_qp_num`: remote QP number.
    /// `rq_psn`: starting PSN for receive.
    /// `dgid`: destination GID.
    /// `port_num`: local port.
    pub fn to_rtr(
        &self,
        dest_qp_num: u32,
        rq_psn: u32,
        dgid: &ibv_gid,
        port_num: u8,
    ) -> Result<()> {
        let mut attr = ibv_qp_attr {
            qp_state: IBV_QPS_RTR,
            path_mtu: IBV_MTU_1024,
            dest_qp_num,
            rq_psn,
            max_dest_rd_atomic: 1,
            min_rnr_timer: 12,
            ah_attr: ibv_ah_attr {
                grh: ibv_global_route {
                    dgid: *dgid,
                    sgid_index: 0,
                    hop_limit: 1,
                    ..Default::default()
                },
                dlid: 0,
                sl: 0,
                src_path_bits: 0,
                is_global: 1,
                port_num,
                ..Default::default()
            },
            ..Default::default()
        };
        let mask = IBV_QP_STATE
            | IBV_QP_AV
            | IBV_QP_PATH_MTU
            | IBV_QP_DEST_QPN
            | IBV_QP_RQ_PSN
            | IBV_QP_MAX_DEST_RD_ATOMIC
            | IBV_QP_MIN_RNR_TIMER;
        self.modify(&mut attr, mask)
    }

    /// Transition QP to RTS state.
    pub fn to_rts(&self, sq_psn: u32) -> Result<()> {
        let mut attr = ibv_qp_attr {
            qp_state: IBV_QPS_RTS,
            timeout: 14,
            retry_cnt: 7,
            rnr_retry: 7,
            sq_psn,
            max_rd_atomic: 1,
            ..Default::default()
        };
        let mask = IBV_QP_STATE
            | IBV_QP_TIMEOUT
            | IBV_QP_RETRY_CNT
            | IBV_QP_RNR_RETRY
            | IBV_QP_SQ_PSN
            | IBV_QP_MAX_QP_RD_ATOMIC;
        self.modify(&mut attr, mask)
    }

    /// Post a send work request (legacy API).
    pub fn post_send(&self, wr: &mut SendWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_send(self.inner, &mut raw, &mut bad_wr) })
    }

    /// Post a receive work request (legacy API).
    pub fn post_recv(&self, wr: &mut RecvWr) -> Result<()> {
        let mut raw = wr.build_raw();
        let mut bad_wr: *mut ibv_recv_wr = std::ptr::null_mut();
        from_ret(unsafe { rdma_wrap_ibv_post_recv(self.inner, &mut raw, &mut bad_wr) })
    }

    /// Query the QP.
    pub fn query(&self, attr_mask: u32) -> Result<(ibv_qp_attr, ibv_qp_init_attr)> {
        let mut attr = ibv_qp_attr::default();
        let mut init_attr = ibv_qp_init_attr::default();
        from_ret(unsafe { ibv_query_qp(self.inner, &mut attr, attr_mask as i32, &mut init_attr) })?;
        Ok((attr, init_attr))
    }

    /// Raw pointer (for advanced/FFI use).
    pub fn as_raw(&self) -> *mut ibv_qp {
        self.inner
    }
}
