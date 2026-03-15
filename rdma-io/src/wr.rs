//! Work Request builders and related types.

use rdma_io_sys::ibverbs::*;

/// QP type enum (typed wrapper over `ibv_qp_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpType {
    Rc,
    Uc,
    Ud,
    XrcSend,
    XrcRecv,
    RawPacket,
    Driver,
}

impl QpType {
    /// Convert to the raw `ibv_qp_type` constant.
    pub fn as_raw(self) -> u32 {
        match self {
            Self::Rc => IBV_QPT_RC,
            Self::Uc => IBV_QPT_UC,
            Self::Ud => IBV_QPT_UD,
            Self::XrcSend => IBV_QPT_XRC_SEND,
            Self::XrcRecv => IBV_QPT_XRC_RECV,
            Self::RawPacket => IBV_QPT_RAW_PACKET,
            Self::Driver => IBV_QPT_DRIVER,
        }
    }

    /// Convert from a raw `ibv_qp_type` value.
    pub fn from_raw(v: u32) -> Option<Self> {
        match v {
            IBV_QPT_RC => Some(Self::Rc),
            IBV_QPT_UC => Some(Self::Uc),
            IBV_QPT_UD => Some(Self::Ud),
            IBV_QPT_XRC_SEND => Some(Self::XrcSend),
            IBV_QPT_XRC_RECV => Some(Self::XrcRecv),
            IBV_QPT_RAW_PACKET => Some(Self::RawPacket),
            IBV_QPT_DRIVER => Some(Self::Driver),
            _ => None,
        }
    }
}

/// QP state enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpState {
    Reset,
    Init,
    Rtr,
    Rts,
    Sqd,
    Sqe,
    Err,
    Unknown,
}

impl QpState {
    /// Convert to raw `ibv_qp_state`.
    pub fn as_raw(self) -> u32 {
        match self {
            Self::Reset => IBV_QPS_RESET,
            Self::Init => IBV_QPS_INIT,
            Self::Rtr => IBV_QPS_RTR,
            Self::Rts => IBV_QPS_RTS,
            Self::Sqd => IBV_QPS_SQD,
            Self::Sqe => IBV_QPS_SQE,
            Self::Err => IBV_QPS_ERR,
            Self::Unknown => IBV_QPS_UNKNOWN,
        }
    }

    /// Convert from raw value.
    pub fn from_raw(v: u32) -> Self {
        match v {
            IBV_QPS_RESET => Self::Reset,
            IBV_QPS_INIT => Self::Init,
            IBV_QPS_RTR => Self::Rtr,
            IBV_QPS_RTS => Self::Rts,
            IBV_QPS_SQD => Self::Sqd,
            IBV_QPS_SQE => Self::Sqe,
            IBV_QPS_ERR => Self::Err,
            _ => Self::Unknown,
        }
    }
}

bitflags::bitflags! {
    /// Send flags for work requests.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SendFlags: u32 {
        const FENCE = IBV_SEND_FENCE;
        const SIGNALED = IBV_SEND_SIGNALED;
        const SOLICITED = IBV_SEND_SOLICITED;
        const INLINE = IBV_SEND_INLINE;
        const IP_CSUM = IBV_SEND_IP_CSUM;
    }
}

/// Scatter-Gather Entry — describes a memory buffer for a WR.
#[repr(transparent)]
#[derive(Clone, Copy, Default)]
pub struct Sge {
    pub(crate) inner: ibv_sge,
}

impl std::fmt::Debug for Sge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sge")
            .field("addr", &self.inner.addr)
            .field("length", &self.inner.length)
            .field("lkey", &self.inner.lkey)
            .finish()
    }
}

impl Sge {
    /// Create a new SGE.
    pub fn new(addr: u64, length: u32, lkey: u32) -> Self {
        Self {
            inner: ibv_sge { addr, length, lkey },
        }
    }
}

/// Builder for a receive work request.
pub struct RecvWr {
    pub(crate) wr_id: u64,
    pub(crate) sges: Vec<Sge>,
}

impl RecvWr {
    /// Create a new receive WR with the given WR id.
    pub fn new(wr_id: u64) -> Self {
        Self {
            wr_id,
            sges: Vec::new(),
        }
    }

    /// Add a scatter-gather entry.
    pub fn sg(mut self, sge: Sge) -> Self {
        self.sges.push(sge);
        self
    }

    /// Build the raw `ibv_recv_wr`. The caller must ensure `sges` outlives usage.
    pub(crate) fn build_raw(&mut self) -> ibv_recv_wr {
        ibv_recv_wr {
            wr_id: self.wr_id,
            next: std::ptr::null_mut(),
            sg_list: if self.sges.is_empty() {
                std::ptr::null_mut()
            } else {
                self.sges.as_mut_ptr().cast()
            },
            num_sge: self.sges.len() as i32,
        }
    }
}

/// Opcode for send work requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrOpcode {
    Send,
    SendWithImm(u32),
    RdmaWrite,
    RdmaWriteWithImm(u32),
    RdmaRead,
    AtomicCmpAndSwp,
    AtomicFetchAndAdd,
    /// Bind a Memory Window to an MR sub-region (Type 2).
    BindMw,
    /// Invalidate a Memory Window's rkey (makes it unusable for remote access).
    LocalInv,
}

impl WrOpcode {
    fn as_raw(self) -> u32 {
        match self {
            Self::Send => IBV_WR_SEND,
            Self::SendWithImm(_) => IBV_WR_SEND_WITH_IMM,
            Self::RdmaWrite => IBV_WR_RDMA_WRITE,
            Self::RdmaWriteWithImm(_) => IBV_WR_RDMA_WRITE_WITH_IMM,
            Self::RdmaRead => IBV_WR_RDMA_READ,
            Self::AtomicCmpAndSwp => IBV_WR_ATOMIC_CMP_AND_SWP,
            Self::AtomicFetchAndAdd => IBV_WR_ATOMIC_FETCH_AND_ADD,
            Self::BindMw => IBV_WR_BIND_MW,
            Self::LocalInv => IBV_WR_LOCAL_INV,
        }
    }
}

/// Builder for a send work request.
pub struct SendWr {
    pub(crate) wr_id: u64,
    pub(crate) opcode: WrOpcode,
    pub(crate) send_flags: SendFlags,
    pub(crate) sges: Vec<Sge>,
    pub(crate) rdma_remote_addr: u64,
    pub(crate) rdma_rkey: u32,
    pub(crate) atomic_compare_add: u64,
    pub(crate) atomic_swap: u64,
    // MW bind fields (for BindMw opcode)
    pub(crate) bind_mw_mw: *mut ibv_mw,
    pub(crate) bind_mw_rkey: u32,
    pub(crate) bind_mw_bind_info: ibv_mw_bind_info,
    // Local invalidation (for LocalInv opcode)
    pub(crate) invalidate_rkey: u32,
}

// Safety: The raw pointers (*mut ibv_mw, *mut ibv_mr in bind_info) are RDMA
// kernel-managed handles, safe to send between threads — same justification
// as OwnedMemoryRegion which also holds *mut ibv_mr.
unsafe impl Send for SendWr {}

impl SendWr {
    /// Create a new send WR.
    pub fn new(wr_id: u64, opcode: WrOpcode) -> Self {
        Self {
            wr_id,
            opcode,
            send_flags: SendFlags::empty(),
            sges: Vec::new(),
            rdma_remote_addr: 0,
            rdma_rkey: 0,
            atomic_compare_add: 0,
            atomic_swap: 0,
            bind_mw_mw: std::ptr::null_mut(),
            bind_mw_rkey: 0,
            bind_mw_bind_info: ibv_mw_bind_info::default(),
            invalidate_rkey: 0,
        }
    }

    /// Set send flags.
    pub fn flags(mut self, flags: SendFlags) -> Self {
        self.send_flags = flags;
        self
    }

    /// Add a scatter-gather entry.
    pub fn sg(mut self, sge: Sge) -> Self {
        self.sges.push(sge);
        self
    }

    /// Set RDMA remote address and rkey (for RDMA read/write ops).
    pub fn rdma(mut self, remote_addr: u64, rkey: u32) -> Self {
        self.rdma_remote_addr = remote_addr;
        self.rdma_rkey = rkey;
        self
    }

    /// Set atomic operation parameters (for CAS and FAA).
    pub fn atomic(mut self, remote_addr: u64, rkey: u32, compare_add: u64, swap: u64) -> Self {
        self.rdma_remote_addr = remote_addr;
        self.rdma_rkey = rkey;
        self.atomic_compare_add = compare_add;
        self.atomic_swap = swap;
        self
    }

    /// Set Memory Window bind parameters (for BindMw opcode).
    ///
    /// # Arguments
    /// * `mw` - Raw MW pointer to bind
    /// * `rkey` - New rkey to assign to the MW after binding
    /// * `mr` - Raw MR pointer that the MW will be bound to
    /// * `addr` - Start address within the MR
    /// * `length` - Length of the bound region
    /// * `access` - Access flags for the MW binding
    pub fn bind_mw(
        mut self,
        mw: *mut ibv_mw,
        rkey: u32,
        mr: *mut ibv_mr,
        addr: u64,
        length: u64,
        access: u32,
    ) -> Self {
        self.bind_mw_mw = mw;
        self.bind_mw_rkey = rkey;
        self.bind_mw_bind_info = ibv_mw_bind_info {
            mr,
            addr,
            length,
            mw_access_flags: access,
        };
        self
    }

    /// Set the rkey to invalidate (for LocalInv opcode).
    pub fn inv_rkey(mut self, rkey: u32) -> Self {
        self.invalidate_rkey = rkey;
        self
    }

    /// Build the raw `ibv_send_wr`. The caller must ensure `sges` outlives usage.
    pub(crate) fn build_raw(&mut self) -> ibv_send_wr {
        let sg_list = if self.sges.is_empty() {
            std::ptr::null_mut()
        } else {
            self.sges.as_mut_ptr().cast()
        };
        let mut wr = ibv_send_wr {
            wr_id: self.wr_id,
            opcode: self.opcode.as_raw(),
            send_flags: self.send_flags.bits(),
            sg_list,
            num_sge: self.sges.len() as i32,
            next: std::ptr::null_mut(),
            ..Default::default()
        };

        // Set immediate data if applicable.
        match self.opcode {
            WrOpcode::SendWithImm(imm) | WrOpcode::RdmaWriteWithImm(imm) => {
                wr.ibv_send_wr__anon_0.imm_data = imm;
            }
            WrOpcode::LocalInv => {
                wr.ibv_send_wr__anon_0.invalidate_rkey = self.invalidate_rkey;
            }
            _ => {}
        }

        // Set RDMA fields.
        match self.opcode {
            WrOpcode::RdmaWrite | WrOpcode::RdmaWriteWithImm(_) | WrOpcode::RdmaRead => {
                wr.wr.rdma = ibv_send_wr_wr_rdma {
                    remote_addr: self.rdma_remote_addr,
                    rkey: self.rdma_rkey,
                };
            }
            WrOpcode::AtomicCmpAndSwp | WrOpcode::AtomicFetchAndAdd => {
                wr.wr.atomic = ibv_send_wr_wr_atomic {
                    remote_addr: self.rdma_remote_addr,
                    compare_add: self.atomic_compare_add,
                    swap: self.atomic_swap,
                    rkey: self.rdma_rkey,
                };
            }
            WrOpcode::BindMw => {
                wr.ibv_send_wr__anon_1.bind_mw = ibv_send_wr__anon_1_bind_mw {
                    mw: self.bind_mw_mw,
                    rkey: self.bind_mw_rkey,
                    bind_info: self.bind_mw_bind_info,
                };
            }
            _ => {}
        }

        wr
    }
}
