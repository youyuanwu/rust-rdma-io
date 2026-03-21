//! Shared ring-buffer infrastructure for RDMA ring transports.
//!
//! Contains types and helpers used by both
//! [`CreditRingTransport`](crate::credit_ring_transport::CreditRingTransport)
//! and future ring transports: ring tokens, ring buffers, completion tracking,
//! memory window binding, token exchange, and QP state checks.

use std::sync::Arc;
use std::time::Duration;

use crate::async_qp::AsyncQp;
use crate::mr::{AccessFlags, OwnedMemoryRegion};
use crate::mw::MemoryWindow;
use crate::pd::ProtectionDomain;
use crate::wc::WorkCompletion;
use crate::wr::{RecvWr, SendFlags, SendWr, Sge, WrOpcode};

/// wr_id bit 63: 0 = data WR, 1 = credit WR.
pub(crate) const WR_ID_CREDIT_FLAG: u64 = 1 << 63;
/// Sentinel wr_id for padding WRs — no send_ring data to free on completion.
pub(crate) const WR_ID_PADDING_SENTINEL: u64 = u64::MAX - 20;

pub(crate) const RING_TOKEN_VERSION: u8 = 1;
pub(crate) const RING_TOKEN_SIZE: usize = 20;

// ---------------------------------------------------------------------------
// RingToken — 20-byte connection setup token
// ---------------------------------------------------------------------------

/// 20-byte token exchanged during ring transport connection setup.
///
/// Contains the remote ring's virtual address, memory window rkey, and capacity.
#[repr(C, packed)]
pub(crate) struct RingToken {
    /// Protocol version (1 = small-ring mode).
    pub(crate) version: u8,
    pub(crate) _reserved: [u8; 3],
    /// Base virtual address of recv ring.
    pub(crate) ring_va: u64,
    /// Memory Window rkey (scoped to recv ring).
    pub(crate) mw_rkey: u32,
    /// Ring buffer size in bytes.
    pub(crate) capacity: u32,
}

// Compile-time size check.
const _: () = assert!(std::mem::size_of::<RingToken>() == RING_TOKEN_SIZE);

impl RingToken {
    pub(crate) fn to_bytes(&self) -> [u8; RING_TOKEN_SIZE] {
        let mut buf = [0u8; RING_TOKEN_SIZE];
        buf[0] = self.version;
        // bytes 1..4 reserved (zero)
        buf[4..12].copy_from_slice(&self.ring_va.to_le_bytes());
        buf[12..16].copy_from_slice(&self.mw_rkey.to_le_bytes());
        buf[16..20].copy_from_slice(&self.capacity.to_le_bytes());
        buf
    }

    pub(crate) fn from_bytes(buf: &[u8; RING_TOKEN_SIZE]) -> Self {
        Self {
            version: buf[0],
            _reserved: [buf[1], buf[2], buf[3]],
            ring_va: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
            mw_rkey: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            capacity: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
        }
    }
}

// ---------------------------------------------------------------------------
// RingBuffer — wrap-around ring for RDMA writes
// ---------------------------------------------------------------------------

pub(crate) struct RingBuffer {
    pub(crate) mr: OwnedMemoryRegion,
    pub(crate) capacity: usize,
    pub(crate) head: usize,
    pub(crate) tail: usize,
}

impl RingBuffer {
    pub(crate) fn new(mr: OwnedMemoryRegion, capacity: usize) -> Self {
        Self {
            mr,
            capacity,
            head: 0,
            tail: 0,
        }
    }

    /// Total free space in the ring.
    pub(crate) fn available(&self) -> usize {
        // We keep one byte unused to distinguish full from empty.
        self.capacity - self.used() - 1
    }

    /// Bytes currently occupied.
    pub(crate) fn used(&self) -> usize {
        if self.tail >= self.head {
            self.tail - self.head
        } else {
            self.capacity - self.head + self.tail
        }
    }

    /// Contiguous free space at tail (before wrap).
    pub(crate) fn contiguous_free(&self) -> usize {
        if self.tail >= self.head {
            // Tail is at or after head — free space extends to end of buffer.
            // If head == 0, we must leave 1 byte gap, so cap - tail - 1.
            if self.head == 0 {
                self.capacity - self.tail - 1
            } else {
                self.capacity - self.tail
            }
        } else {
            // Tail wrapped — free space is between tail and head.
            self.head - self.tail - 1
        }
    }

    /// Reserve `len` bytes in the ring. Returns `(offset, padding_len)`.
    ///
    /// If there is not enough contiguous space at the tail but enough after
    /// wrapping, returns padding > 0 meaning the caller must send a padding
    /// marker to fill the gap. Returns `None` if not enough total space.
    pub(crate) fn reserve(&mut self, len: usize) -> Option<(usize, usize)> {
        if len == 0 || len > self.available() {
            return None;
        }

        let contig = self.contiguous_free();
        if len <= contig {
            let offset = self.tail;
            self.tail = (self.tail + len) % self.capacity;
            Some((offset, 0))
        } else {
            // Need to wrap — check if data fits at the beginning.
            // Padding fills tail..capacity, data goes at 0..len.
            let padding = self.capacity - self.tail;
            // After padding, tail wraps to 0. Check space: need padding + len total.
            if padding + len > self.available() + 1 {
                return None;
            }
            // head must be > len (since tail will be at 0..len after wrap).
            if self.head <= len {
                return None;
            }
            let offset = 0;
            self.tail = len;
            Some((offset, padding))
        }
    }

    /// Advance head by `len` bytes (consumer releases data).
    pub(crate) fn release(&mut self, len: usize) {
        self.head = (self.head + len) % self.capacity;
    }
}

// ---------------------------------------------------------------------------
// CompletionTracker — chase-forward slot release
// ---------------------------------------------------------------------------

pub(crate) struct CompletionTracker {
    released: Box<[bool]>,
    head_slot: usize,
    capacity: usize,
}

impl CompletionTracker {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            released: vec![false; capacity].into_boxed_slice(),
            head_slot: 0,
            capacity,
        }
    }

    /// Mark `slot` as released and chase forward from head_slot.
    ///
    /// Returns the number of contiguous slots that can be advanced.
    pub(crate) fn release(&mut self, slot: usize) -> usize {
        if slot < self.capacity {
            self.released[slot] = true;
        }
        let mut advanced = 0;
        while self.released[self.head_slot] {
            self.released[self.head_slot] = false;
            self.head_slot = (self.head_slot + 1) % self.capacity;
            advanced += 1;
        }
        advanced
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Allocate and bind a Memory Window Type 2 to the recv ring MR.
/// Must be called AFTER QP reaches RTS (post-connect/accept).
/// Panics if the device does not support MW Type 2.
pub(crate) fn bind_recv_mw(
    qp: &AsyncQp,
    pd: &Arc<ProtectionDomain>,
    recv_mr: &OwnedMemoryRegion,
    ring_capacity: usize,
) -> crate::Result<(MemoryWindow, u32)> {
    assert!(
        crate::device::supports_mw_type2(pd),
        "ring transport requires Memory Window Type 2 support \
         (device does not support ibv_alloc_mw Type 2)"
    );

    let mw = MemoryWindow::alloc(pd, crate::mw::MwType::Type2)?;
    let mw_rkey = mw.rkey();

    // Bind MW to the recv ring MR region via IBV_WR_BIND_MW send WR.
    let mut bind_wr = SendWr::new(u64::MAX - 10, WrOpcode::BindMw)
        .flags(SendFlags::SIGNALED)
        .bind_mw(
            mw.as_raw(),
            mw_rkey,
            recv_mr.as_raw(),
            recv_mr.addr(),
            ring_capacity as u64,
            rdma_io_sys::ibverbs::IBV_ACCESS_REMOTE_WRITE,
        );
    qp.post_send_wr(&mut bind_wr)?;

    // The bind completion will be drained by drain_send_cq after token exchange.
    // After bind, the MW rkey grants scoped REMOTE_WRITE to exactly the recv ring.
    Ok((mw, mw_rkey))
}

/// Allocate token recv MR and post the recv WR. Must be called BEFORE connect/accept
/// so it's the first recv WR on the QP (consumed by the peer's token Send).
pub(crate) fn post_token_recv(
    qp: &AsyncQp,
    pd: &Arc<ProtectionDomain>,
) -> crate::Result<OwnedMemoryRegion> {
    let token_recv_mr = pd.reg_mr_owned(vec![0u8; RING_TOKEN_SIZE], AccessFlags::LOCAL_WRITE)?;
    let recv_sge = Sge::new(
        token_recv_mr.addr(),
        RING_TOKEN_SIZE as u32,
        token_recv_mr.lkey(),
    );
    let mut recv_wr = RecvWr::new(u64::MAX).sg(recv_sge);
    qp.post_recv_wr(&mut recv_wr)?;
    Ok(token_recv_mr)
}

/// Send our token and asynchronously wait for the peer's token to arrive.
/// Called AFTER connect/accept (QP is RTS, peer's token Send is in flight).
pub(crate) async fn complete_token_exchange(
    qp: &AsyncQp,
    pd: &Arc<ProtectionDomain>,
    recv_mr: &OwnedMemoryRegion,
    mw_rkey: u32,
    token_recv_mr: &OwnedMemoryRegion,
    _token_timeout: Duration,
    ring_capacity: usize,
) -> crate::Result<(u64, u32, usize)> {
    // Send our token.
    let our_token = RingToken {
        version: RING_TOKEN_VERSION,
        _reserved: [0; 3],
        ring_va: recv_mr.addr(),
        mw_rkey,
        capacity: ring_capacity as u32,
    };
    let token_bytes = our_token.to_bytes();

    let token_send_mr = pd.reg_mr_owned(token_bytes.to_vec(), AccessFlags::LOCAL_WRITE)?;
    let send_sge = Sge::new(
        token_send_mr.addr(),
        RING_TOKEN_SIZE as u32,
        token_send_mr.lkey(),
    );
    let mut send_wr = SendWr::new(u64::MAX - 2, WrOpcode::Send)
        .flags(SendFlags::SIGNALED | SendFlags::INLINE)
        .sg(send_sge);
    qp.post_send_wr(&mut send_wr)?;

    // Async wait for recv completion via CQ notification.
    let mut wc_buf = [WorkCompletion::default(); 4];
    let n = qp.recv_cq().poll(&mut wc_buf).await?;
    if n > 0 && !wc_buf[0].is_success() {
        return Err(crate::Error::WorkCompletion {
            status: wc_buf[0].status_raw(),
            vendor_err: wc_buf[0].vendor_err(),
        });
    }

    // Parse received token.
    let recv_buf: &[u8; RING_TOKEN_SIZE] = token_recv_mr
        .as_slice()
        .try_into()
        .expect("token recv MR is exactly RING_TOKEN_SIZE");
    let peer_token = RingToken::from_bytes(recv_buf);

    let peer_ver = peer_token.version;
    if peer_ver != RING_TOKEN_VERSION {
        return Err(crate::Error::InvalidArg(format!(
            "unsupported ring token version: {peer_ver}",
        )));
    }
    let peer_cap = peer_token.capacity;
    if peer_cap == 0 {
        return Err(crate::Error::InvalidArg("peer ring capacity is 0".into()));
    }
    if peer_cap as usize > 65536 {
        return Err(crate::Error::InvalidArg(format!(
            "peer ring capacity too large: {peer_cap}",
        )));
    }
    Ok((peer_token.ring_va, peer_token.mw_rkey, peer_cap as usize))
}

/// Drain all pending completions from the send CQ.
/// Waits briefly for in-flight completions (e.g., token Send) to arrive.
/// Returns the number of completions drained.
pub(crate) fn drain_send_cq(qp: &AsyncQp) -> crate::Result<usize> {
    let mut wc = [WorkCompletion::default(); 16];
    let mut total = 0;
    // Brief spin to catch in-flight completions from setup WRs.
    for _ in 0..100 {
        qp.send_cq().cq().req_notify(false)?;
        match qp.send_cq().cq().poll(&mut wc) {
            Ok(0) => {
                if total > 0 {
                    break; // Already drained some, CQ is empty now.
                }
                std::hint::spin_loop();
            }
            Ok(n) => {
                total += n;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// Check whether the QP has left RTS state (e.g. entered ERROR).
pub(crate) fn is_qp_dead(qp: *mut rdma_io_sys::ibverbs::ibv_qp) -> bool {
    unsafe {
        let mut attr = std::mem::MaybeUninit::<rdma_io_sys::ibverbs::ibv_qp_attr>::uninit();
        let mut init_attr =
            std::mem::MaybeUninit::<rdma_io_sys::ibverbs::ibv_qp_init_attr>::uninit();
        let ret = rdma_io_sys::ibverbs::ibv_query_qp(
            qp,
            attr.as_mut_ptr(),
            rdma_io_sys::ibverbs::IBV_QP_STATE as i32,
            init_attr.as_mut_ptr(),
        );
        ret != 0 || (*attr.as_ptr()).qp_state != rdma_io_sys::ibverbs::IBV_QPS_RTS
    }
}
