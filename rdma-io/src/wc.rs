//! Work Completion types.

use rdma_io_sys::ibverbs::*;

/// A work completion entry.
///
/// Thin wrapper around `ibv_wc` with typed accessors.
#[repr(transparent)]
#[derive(Clone, Copy, Default)]
pub struct WorkCompletion {
    pub(crate) inner: ibv_wc,
}

impl WorkCompletion {
    /// The WR id that was completed.
    pub fn wr_id(&self) -> u64 {
        self.inner.wr_id
    }

    /// Raw status value.
    pub fn status_raw(&self) -> u32 {
        self.inner.status
    }

    /// Whether this completion is successful.
    pub fn is_success(&self) -> bool {
        self.inner.status == IBV_WC_SUCCESS
    }

    /// Typed status.
    pub fn status(&self) -> WcStatus {
        WcStatus::from_raw(self.inner.status)
    }

    /// Typed opcode.
    pub fn opcode(&self) -> WcOpcode {
        WcOpcode::from_raw(self.inner.opcode)
    }

    /// Vendor-specific error code.
    pub fn vendor_err(&self) -> u32 {
        self.inner.vendor_err
    }

    /// Number of bytes transferred (for recv completions).
    pub fn byte_len(&self) -> u32 {
        self.inner.byte_len
    }

    /// QP number that generated this completion.
    pub fn qp_num(&self) -> u32 {
        self.inner.qp_num
    }

    /// Source QP number (for recv completions on UD QPs).
    pub fn src_qp(&self) -> u32 {
        self.inner.src_qp
    }

    /// WC flags.
    pub fn wc_flags(&self) -> u32 {
        self.inner.wc_flags
    }

    /// Immediate data (valid if `wc_flags` has `IBV_WC_WITH_IMM`).
    pub fn imm_data(&self) -> u32 {
        unsafe { self.inner.ibv_wc__anon_0.imm_data }
    }

    /// Raw `ibv_wc` reference.
    pub fn as_raw(&self) -> &ibv_wc {
        &self.inner
    }
}

impl std::fmt::Debug for WorkCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkCompletion")
            .field("wr_id", &self.wr_id())
            .field("status", &self.status())
            .field("opcode", &self.opcode())
            .field("byte_len", &self.byte_len())
            .field("qp_num", &self.qp_num())
            .finish()
    }
}

/// Work completion status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WcStatus {
    Success,
    LocLenErr,
    LocQpOpErr,
    LocEecOpErr,
    LocProtErr,
    WrFlushErr,
    MwBindErr,
    BadRespErr,
    LocAccessErr,
    RemInvReqErr,
    RemAccessErr,
    RemOpErr,
    RetryExcErr,
    RnrRetryExcErr,
    LocRddViolErr,
    RemAbortErr,
    InvEecnErr,
    InvEecStateErr,
    FatalErr,
    RespTimeoutErr,
    GeneralErr,
    TmErr,
    TmRndvIncomplete,
    Unknown(u32),
}

impl WcStatus {
    /// Convert from raw `ibv_wc_status` value.
    pub fn from_raw(v: u32) -> Self {
        match v {
            IBV_WC_SUCCESS => Self::Success,
            IBV_WC_LOC_LEN_ERR => Self::LocLenErr,
            IBV_WC_LOC_QP_OP_ERR => Self::LocQpOpErr,
            IBV_WC_LOC_EEC_OP_ERR => Self::LocEecOpErr,
            IBV_WC_LOC_PROT_ERR => Self::LocProtErr,
            IBV_WC_WR_FLUSH_ERR => Self::WrFlushErr,
            IBV_WC_MW_BIND_ERR => Self::MwBindErr,
            IBV_WC_BAD_RESP_ERR => Self::BadRespErr,
            IBV_WC_LOC_ACCESS_ERR => Self::LocAccessErr,
            IBV_WC_REM_INV_REQ_ERR => Self::RemInvReqErr,
            IBV_WC_REM_ACCESS_ERR => Self::RemAccessErr,
            IBV_WC_REM_OP_ERR => Self::RemOpErr,
            IBV_WC_RETRY_EXC_ERR => Self::RetryExcErr,
            IBV_WC_RNR_RETRY_EXC_ERR => Self::RnrRetryExcErr,
            IBV_WC_LOC_RDD_VIOL_ERR => Self::LocRddViolErr,
            IBV_WC_REM_ABORT_ERR => Self::RemAbortErr,
            IBV_WC_INV_EECN_ERR => Self::InvEecnErr,
            IBV_WC_INV_EEC_STATE_ERR => Self::InvEecStateErr,
            IBV_WC_FATAL_ERR => Self::FatalErr,
            IBV_WC_RESP_TIMEOUT_ERR => Self::RespTimeoutErr,
            IBV_WC_GENERAL_ERR => Self::GeneralErr,
            IBV_WC_TM_ERR => Self::TmErr,
            IBV_WC_TM_RNDV_INCOMPLETE => Self::TmRndvIncomplete,
            other => Self::Unknown(other),
        }
    }
}

/// Work completion opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WcOpcode {
    Send,
    RdmaWrite,
    RdmaRead,
    CompSwap,
    FetchAdd,
    BindMw,
    LocalInv,
    Tso,
    Flush,
    AtomicWrite,
    Recv,
    RecvRdmaWithImm,
    TmAdd,
    TmDel,
    TmSync,
    TmRecv,
    TmNoTag,
    Unknown(u32),
}

impl WcOpcode {
    /// Convert from raw `ibv_wc_opcode` value.
    pub fn from_raw(v: u32) -> Self {
        match v {
            IBV_WC_SEND => Self::Send,
            IBV_WC_RDMA_WRITE => Self::RdmaWrite,
            IBV_WC_RDMA_READ => Self::RdmaRead,
            IBV_WC_COMP_SWAP => Self::CompSwap,
            IBV_WC_FETCH_ADD => Self::FetchAdd,
            IBV_WC_BIND_MW => Self::BindMw,
            IBV_WC_LOCAL_INV => Self::LocalInv,
            IBV_WC_TSO => Self::Tso,
            IBV_WC_FLUSH => Self::Flush,
            IBV_WC_ATOMIC_WRITE => Self::AtomicWrite,
            IBV_WC_RECV => Self::Recv,
            IBV_WC_RECV_RDMA_WITH_IMM => Self::RecvRdmaWithImm,
            IBV_WC_TM_ADD => Self::TmAdd,
            IBV_WC_TM_DEL => Self::TmDel,
            IBV_WC_TM_SYNC => Self::TmSync,
            IBV_WC_TM_RECV => Self::TmRecv,
            IBV_WC_TM_NO_TAG => Self::TmNoTag,
            other => Self::Unknown(other),
        }
    }
}
