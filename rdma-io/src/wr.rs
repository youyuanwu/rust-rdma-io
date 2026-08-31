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

/// Stable linked SEND work-request storage for one verbs post call.
pub(crate) struct PreparedSendBatch {
    raw: Box<[ibv_send_wr]>,
    _sges: Box<[ibv_sge]>,
    ledger_indices: Box<[usize]>,
}

impl PreparedSendBatch {
    pub(crate) fn new(mut requests: Vec<SendWr>) -> crate::Result<Self> {
        if requests.is_empty() {
            return Err(crate::Error::InvalidArg(
                "SEND batch must not be empty".into(),
            ));
        }
        let total_sges = requests.iter().try_fold(0usize, |total, request| {
            i32::try_from(request.sges.len())
                .map_err(|_| crate::Error::InvalidArg("SEND SGE count does not fit i32".into()))?;
            total
                .checked_add(request.sges.len())
                .ok_or_else(|| crate::Error::InvalidArg("SEND batch SGE count overflow".into()))
        })?;

        let mut sges = Vec::new();
        sges.try_reserve_exact(total_sges)
            .map_err(|_| crate::Error::InvalidArg("SEND batch SGE allocation failed".into()))?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(requests.len())
            .map_err(|_| crate::Error::InvalidArg("SEND batch WR allocation failed".into()))?;
        let mut offsets = Vec::new();
        offsets
            .try_reserve_exact(requests.len())
            .map_err(|_| crate::Error::InvalidArg("SEND batch offset allocation failed".into()))?;

        for request in &mut requests {
            offsets.push(sges.len());
            sges.extend(request.sges.iter().map(|sge| sge.inner));
            raw.push(request.build_raw());
        }

        let mut raw = raw.into_boxed_slice();
        let mut sges = sges.into_boxed_slice();
        for index in 0..raw.len() {
            raw[index].sg_list = if raw[index].num_sge == 0 {
                std::ptr::null_mut()
            } else {
                // The boxed SGE array is at its final stable address.
                unsafe { sges.as_mut_ptr().add(offsets[index]) }
            };
        }
        link_send_nodes(&mut raw);
        let ledger_indices = (0..raw.len()).collect::<Vec<_>>().into_boxed_slice();
        Ok(Self {
            raw,
            _sges: sges,
            ledger_indices,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.raw.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub(crate) fn head_mut(&mut self) -> *mut ibv_send_wr {
        self.raw.as_mut_ptr()
    }

    pub(crate) fn ledger_index(&self, index: usize) -> Option<usize> {
        self.ledger_indices.get(index).copied()
    }

    pub(crate) fn first_unaccepted(&self, bad_wr: *mut ibv_send_wr) -> Option<usize> {
        if !valid_send_chain(&self.raw) {
            return None;
        }
        member_index(self.raw.as_ptr(), self.raw.len(), bad_wr)
    }

    #[cfg(test)]
    pub(crate) fn member_ptr_for_test(&mut self, index: usize) -> *mut ibv_send_wr {
        assert!(index < self.raw.len());
        unsafe { self.raw.as_mut_ptr().add(index) }
    }

    #[cfg(test)]
    pub(crate) fn wr_id_for_test(&self, index: usize) -> u64 {
        self.raw[index].wr_id
    }

    #[cfg(test)]
    pub(crate) fn make_cycle_for_test(&mut self) {
        let head = self.raw.as_mut_ptr();
        self.raw.last_mut().unwrap().next = head;
    }
}

/// Stable linked RECV work-request storage for one verbs post call.
pub(crate) struct PreparedRecvBatch {
    raw: Box<[ibv_recv_wr]>,
    _sges: Box<[ibv_sge]>,
    ledger_indices: Box<[usize]>,
}

impl PreparedRecvBatch {
    pub(crate) fn new(mut requests: Vec<RecvWr>) -> crate::Result<Self> {
        if requests.is_empty() {
            return Err(crate::Error::InvalidArg(
                "RECV batch must not be empty".into(),
            ));
        }
        let total_sges = requests.iter().try_fold(0usize, |total, request| {
            i32::try_from(request.sges.len())
                .map_err(|_| crate::Error::InvalidArg("RECV SGE count does not fit i32".into()))?;
            total
                .checked_add(request.sges.len())
                .ok_or_else(|| crate::Error::InvalidArg("RECV batch SGE count overflow".into()))
        })?;

        let mut sges = Vec::new();
        sges.try_reserve_exact(total_sges)
            .map_err(|_| crate::Error::InvalidArg("RECV batch SGE allocation failed".into()))?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(requests.len())
            .map_err(|_| crate::Error::InvalidArg("RECV batch WR allocation failed".into()))?;
        let mut offsets = Vec::new();
        offsets
            .try_reserve_exact(requests.len())
            .map_err(|_| crate::Error::InvalidArg("RECV batch offset allocation failed".into()))?;

        for request in &mut requests {
            offsets.push(sges.len());
            sges.extend(request.sges.iter().map(|sge| sge.inner));
            raw.push(request.build_raw());
        }

        let mut raw = raw.into_boxed_slice();
        let mut sges = sges.into_boxed_slice();
        for index in 0..raw.len() {
            raw[index].sg_list = if raw[index].num_sge == 0 {
                std::ptr::null_mut()
            } else {
                // The boxed SGE array is at its final stable address.
                unsafe { sges.as_mut_ptr().add(offsets[index]) }
            };
        }
        link_recv_nodes(&mut raw);
        let ledger_indices = (0..raw.len()).collect::<Vec<_>>().into_boxed_slice();
        Ok(Self {
            raw,
            _sges: sges,
            ledger_indices,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.raw.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub(crate) fn head_mut(&mut self) -> *mut ibv_recv_wr {
        self.raw.as_mut_ptr()
    }

    pub(crate) fn ledger_index(&self, index: usize) -> Option<usize> {
        self.ledger_indices.get(index).copied()
    }

    pub(crate) fn first_unaccepted(&self, bad_wr: *mut ibv_recv_wr) -> Option<usize> {
        if !valid_recv_chain(&self.raw) {
            return None;
        }
        member_index(self.raw.as_ptr(), self.raw.len(), bad_wr)
    }

    #[cfg(test)]
    pub(crate) fn member_ptr_for_test(&mut self, index: usize) -> *mut ibv_recv_wr {
        assert!(index < self.raw.len());
        unsafe { self.raw.as_mut_ptr().add(index) }
    }

    #[cfg(test)]
    pub(crate) fn wr_id_for_test(&self, index: usize) -> u64 {
        self.raw[index].wr_id
    }

    #[cfg(test)]
    pub(crate) fn make_cycle_for_test(&mut self) {
        let head = self.raw.as_mut_ptr();
        self.raw.last_mut().unwrap().next = head;
    }
}

fn link_send_nodes(nodes: &mut [ibv_send_wr]) {
    let base = nodes.as_mut_ptr();
    for index in 0..nodes.len() {
        nodes[index].next = if index + 1 == nodes.len() {
            std::ptr::null_mut()
        } else {
            unsafe { base.add(index + 1) }
        };
    }
}

fn link_recv_nodes(nodes: &mut [ibv_recv_wr]) {
    let base = nodes.as_mut_ptr();
    for index in 0..nodes.len() {
        nodes[index].next = if index + 1 == nodes.len() {
            std::ptr::null_mut()
        } else {
            unsafe { base.add(index + 1) }
        };
    }
}

fn valid_send_chain(nodes: &[ibv_send_wr]) -> bool {
    let base = nodes.as_ptr();
    nodes.iter().enumerate().all(|(index, node)| {
        let expected = if index + 1 == nodes.len() {
            std::ptr::null_mut()
        } else {
            unsafe { base.add(index + 1).cast_mut() }
        };
        node.next == expected
    })
}

fn valid_recv_chain(nodes: &[ibv_recv_wr]) -> bool {
    let base = nodes.as_ptr();
    nodes.iter().enumerate().all(|(index, node)| {
        let expected = if index + 1 == nodes.len() {
            std::ptr::null_mut()
        } else {
            unsafe { base.add(index + 1).cast_mut() }
        };
        node.next == expected
    })
}

fn member_index<T>(base: *const T, len: usize, pointer: *mut T) -> Option<usize> {
    if pointer.is_null() {
        return None;
    }
    let size = std::mem::size_of::<T>();
    let start = base as usize;
    let address = pointer as usize;
    let bytes = len.checked_mul(size)?;
    let end = start.checked_add(bytes)?;
    if address < start || address >= end {
        return None;
    }
    let offset = address - start;
    if !offset.is_multiple_of(size) || !address.is_multiple_of(std::mem::align_of::<T>()) {
        return None;
    }
    Some(offset / size)
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    fn sends(count: usize) -> PreparedSendBatch {
        PreparedSendBatch::new(
            (0..count)
                .map(|index| {
                    SendWr::new(index as u64, WrOpcode::Send).sg(Sge::new(
                        index as u64,
                        1,
                        index as u32,
                    ))
                })
                .collect(),
        )
        .unwrap()
    }

    fn recvs(count: usize) -> PreparedRecvBatch {
        PreparedRecvBatch::new(
            (0..count)
                .map(|index| RecvWr::new(index as u64).sg(Sge::new(index as u64, 1, index as u32)))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn prepared_batches_reject_empty_input() {
        assert!(PreparedSendBatch::new(Vec::new()).is_err());
        assert!(PreparedRecvBatch::new(Vec::new()).is_err());
    }

    #[test]
    fn send_bad_wr_membership_is_exact() {
        let mut batch = sends(4);
        let first = batch.member_ptr_for_test(0);
        let third = batch.member_ptr_for_test(2);
        assert_eq!(batch.first_unaccepted(first), Some(0));
        assert_eq!(batch.first_unaccepted(third), Some(2));
        assert_eq!(batch.ledger_index(3), Some(3));
        assert_eq!(batch.len(), 4);
        assert!(!batch.is_empty());

        let foreign = Box::into_raw(Box::new(ibv_send_wr::default()));
        assert_eq!(batch.first_unaccepted(foreign), None);
        unsafe { drop(Box::from_raw(foreign)) };

        let misaligned = (batch.member_ptr_for_test(1) as *mut u8)
            .wrapping_add(1)
            .cast::<ibv_send_wr>();
        assert_eq!(batch.first_unaccepted(misaligned), None);
        assert_eq!(batch.first_unaccepted(std::ptr::null_mut()), None);

        batch.make_cycle_for_test();
        let member = batch.member_ptr_for_test(1);
        assert_eq!(batch.first_unaccepted(member), None);
    }

    #[test]
    fn recv_bad_wr_membership_is_exact() {
        let mut batch = recvs(3);
        let second = batch.member_ptr_for_test(1);
        assert_eq!(batch.first_unaccepted(second), Some(1));
        assert_eq!(batch.ledger_index(2), Some(2));
        assert_eq!(batch.len(), 3);
        assert!(!batch.is_empty());

        let foreign = Box::into_raw(Box::new(ibv_recv_wr::default()));
        assert_eq!(batch.first_unaccepted(foreign), None);
        unsafe { drop(Box::from_raw(foreign)) };

        batch.make_cycle_for_test();
        let member = batch.member_ptr_for_test(0);
        assert_eq!(batch.first_unaccepted(member), None);
    }
}
