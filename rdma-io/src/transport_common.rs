//! Shared ring-buffer infrastructure for RDMA ring transports.
//!
//! Contains types and helpers used by both
//! [`CreditRingTransport`](crate::credit_ring_transport::CreditRingTransport)
//! and future ring transports: ring tokens, ring buffers, completion tracking,
//! memory window binding, token exchange, and QP state checks.

use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;

use crate::async_cm::AsyncCmId;
use crate::async_qp::AsyncQp;
use crate::cm::{CmEventType, CmId, CmQueuePair, EventChannel};
use crate::completion_source::CompletionSource;
use crate::cq::CompletionQueue;
use crate::mr::{AccessFlags, OwnedMemoryRegion};
use crate::mw::MemoryWindow;
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;
use crate::wc::WorkCompletion;
use crate::wr::{RecvWr, SendFlags, SendWr, Sge, WrOpcode};

/// Bounded number of teardown drain sweeps a transport `Drop` spins after
/// forcing the QP to `ERROR`. The outstanding WRs flush locally (microseconds),
/// so this is a short spin, not a wall-clock wait; `ibv_destroy_qp` is the
/// backstop if exhausted. Shared by all three transports.
pub(crate) const TEARDOWN_DRAIN_POLLS: usize = 4096;

/// wr_id bit 63: 0 = data WR, 1 = credit WR.
pub(crate) const WR_ID_CREDIT_FLAG: u64 = 1 << 63;
/// Sentinel wr_id for padding WRs — no send_ring data to free on completion.
pub(crate) const WR_ID_PADDING_SENTINEL: u64 = u64::MAX - 20;
/// Setup wr_id for the recv-ring Memory-Window bind (shared by both ring
/// transports); matched exactly by the read-ring setup-completion drain.
pub(crate) const WR_ID_RECV_MW_BIND: u64 = u64::MAX - 10;
/// Setup wr_id for our token Send (shared token exchange); matched exactly by
/// the setup-completion drain.
pub(crate) const WR_ID_TOKEN_SEND: u64 = u64::MAX - 2;

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

/// Policy for Type-2 Memory Window usage vs. the MR-rkey fallback.
///
/// The ring transports expose remote-accessible memory either through a Type-2
/// Memory Window (per-connection rkey, QP-associated, revocable) or, on NICs
/// that cannot allocate one, directly through the MR's own rkey. This enum
/// selects which — resolved once at connect/accept time via [`resolve_mr_rkey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryWindowMode {
    /// Use Type-2 Memory Windows when the routed device supports them, and
    /// automatically fall back to the MR rkey when it does not (e.g. Azure MANA
    /// reports `max_mw == 0`). This is the default and works everywhere.
    #[default]
    Auto,
    /// Always use Type-2 Memory Windows; fail connect/accept with an error if
    /// the routed device does not support them. Useful in tests/CI to assert
    /// the Memory-Window path is actually exercised (e.g. on rxe).
    Require,
    /// Never allocate a Memory Window; always expose the MR's own rkey.
    Disable,
}

/// Resolve the effective remote-access mode against the routed device.
///
/// Returns `true` when the transport should use the MR rkey (skip Memory
/// Windows), or `false` when it should bind a Type-2 Memory Window. Called once
/// per connection, after the PD is allocated, before any QP setup:
///
/// * [`MemoryWindowMode::Disable`] → always `true` (MR rkey).
/// * [`MemoryWindowMode::Require`] → always `false`, but errors first if the
///   device reports `max_mw == 0` or lacks Type-2 MW support (otherwise
///   `bind_recv_mw` would later fail with an opaque verbs error).
/// * [`MemoryWindowMode::Auto`] → detects support and falls back to `true`
///   when Memory Windows are unavailable.
pub(crate) fn resolve_mr_rkey(
    pd: &Arc<ProtectionDomain>,
    mode: MemoryWindowMode,
) -> crate::Result<bool> {
    match mode {
        MemoryWindowMode::Disable => Ok(true),
        MemoryWindowMode::Require => {
            if !device_supports_mw(pd) {
                return Err(crate::Error::InvalidArg(
                    "MemoryWindowMode::Require: device does not support Type-2 \
                     Memory Windows (max_mw == 0); use MemoryWindowMode::Auto \
                     (the default) to fall back to the MR rkey automatically"
                        .into(),
                ));
            }
            Ok(false)
        }
        MemoryWindowMode::Auto => Ok(!device_supports_mw(pd)),
    }
}

/// Returns `true` if the routed device can allocate Type-2 Memory Windows.
fn device_supports_mw(pd: &Arc<ProtectionDomain>) -> bool {
    let max_mw = pd.context().query_device().map(|a| a.max_mw).unwrap_or(0);
    max_mw != 0 && crate::device::supports_mw_type2(pd)
}

/// Compute the effective in-flight message budget (`max_outstanding`) for a ring
/// transport, decoupled from `max_message_size` when an explicit override is
/// given.
///
/// `max_outstanding` is the number of message *slots* the transport sizes its
/// send queue, recv (doorbell) queue, CQs, and slot-tracking tables to. By
/// default it is `ring_capacity / max_message_size` (each slot budgeted at the
/// max message size). That undercounts small messages: a 64 KiB ring with a
/// 1500 B max message sizes for 43 slots even though ~1024 messages of 64 B
/// could be in flight, so the send/doorbell queues become the bottleneck (see
/// `docs/bugs/ring-send-queue-exhaustion.md`).
///
/// When `max_in_flight` is `Some(n)`, the slot count is `n` instead, letting
/// deep pipelines of small messages use the ring's true byte capacity. The
/// result is clamped to the device's queue limits (`max_qp_wr`, `max_cqe`),
/// leaving `headroom` slots for the extra send WRs each transport posts
/// (padding / RDMA-Read / credit updates), and is always at least 1.
pub(crate) fn effective_max_outstanding(
    pd: &Arc<ProtectionDomain>,
    ring_capacity: usize,
    max_message_size: usize,
    max_in_flight: Option<usize>,
    headroom: usize,
) -> usize {
    let device_limit = pd
        .context()
        .query_device()
        .ok()
        .map(|a| (a.max_qp_wr as usize).min(a.max_cqe as usize))
        .unwrap_or(usize::MAX);
    clamp_max_outstanding(
        ring_capacity,
        max_message_size,
        max_in_flight,
        headroom,
        device_limit,
    )
}

/// Pure slot-count math behind [`effective_max_outstanding`], split out so the
/// clamping/undercount logic can be unit-tested without a device.
///
/// `device_limit` is `min(max_qp_wr, max_cqe)` for the target device (pass
/// [`usize::MAX`] to model an unbounded device). The `send_cq_depth` is
/// `max_outstanding + headroom` and `recv_cq_depth` is `max_outstanding + 2`,
/// so `reserve = headroom.max(2)` slots are held back from `device_limit`.
pub(crate) fn clamp_max_outstanding(
    ring_capacity: usize,
    max_message_size: usize,
    max_in_flight: Option<usize>,
    headroom: usize,
    device_limit: usize,
) -> usize {
    let derived = (ring_capacity / max_message_size.max(1)).max(1);
    let requested = max_in_flight.unwrap_or(derived).max(1);
    // send_cq_depth = max_outstanding + headroom, recv_cq_depth = max_outstanding
    // + 2; both must fit max_qp_wr and max_cqe.
    let reserve = headroom.max(2);
    let device_cap = device_limit.saturating_sub(reserve).max(1);
    requested.min(device_cap).max(1)
}

/// Coalesce a vectored slice list into `dst`, copying up to `dst.len()` bytes
/// total, and return the number of bytes written.
///
/// Transports use this in [`Transport::send_gather`](crate::transport::Transport::send_gather)
/// to gather the caller's slices (e.g. an HTTP/2 frame header plus its payload)
/// straight into the registered send buffer — avoiding the intermediate scratch
/// copy the stream adapter would otherwise need to make the slices contiguous.
pub(crate) fn gather_into(dst: &mut [u8], bufs: &[std::io::IoSlice<'_>]) -> usize {
    let mut written = 0;
    for b in bufs {
        if written >= dst.len() {
            break;
        }
        let take = b.len().min(dst.len() - written);
        dst[written..written + take].copy_from_slice(&b[..take]);
        written += take;
    }
    written
}

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
    let mut bind_wr = SendWr::new(WR_ID_RECV_MW_BIND, WrOpcode::BindMw)
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

/// Post our setup token (`token_bytes`, SIGNALED+INLINE) and await the peer's
/// token recv completion, bounded by `deadline`. The recv WR was posted earlier
/// (`post_token_recv`); each ring transport parses/validates the landed bytes in
/// its own token-recv MR after this returns. Shared post+await for both rings'
/// token exchange (they differ only in token layout + validation).
pub(crate) async fn exchange_setup_token(
    qp: &AsyncQp,
    recv_src: &mut CompletionSource,
    pd: &Arc<ProtectionDomain>,
    token_bytes: &[u8],
    deadline: tokio::time::Instant,
) -> crate::Result<()> {
    let token_send_mr = pd.reg_mr_owned(token_bytes.to_vec(), AccessFlags::LOCAL_WRITE)?;
    let send_sge = Sge::new(
        token_send_mr.addr(),
        token_bytes.len() as u32,
        token_send_mr.lkey(),
    );
    let mut send_wr = SendWr::new(WR_ID_TOKEN_SEND, WrOpcode::Send)
        .flags(SendFlags::SIGNALED | SendFlags::INLINE)
        .sg(send_sge);
    qp.post_send_wr(&mut send_wr)?;

    // Async wait for recv completion via CQ notification (through the seam),
    // bounded by the setup deadline (a peer that establishes CM but never sends
    // its token must not hang setup forever).
    let mut wc_buf = [WorkCompletion::default(); 4];
    let n = match tokio::time::timeout_at(deadline, recv_src.acquire(&mut wc_buf)).await {
        Ok(r) => r?,
        Err(_) => return Err(crate::Error::Timeout("ring peer token")),
    };
    if n > 0 && !wc_buf[0].is_success() {
        return Err(crate::Error::WorkCompletion {
            status: wc_buf[0].status_raw(),
            vendor_err: wc_buf[0].vendor_err(),
        });
    }
    Ok(())
}

/// Send our token and asynchronously wait for the peer's token to arrive.
/// Called AFTER connect/accept (QP is RTS, peer's token Send is in flight).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_token_exchange(
    qp: &AsyncQp,
    recv_src: &mut crate::completion_source::CompletionSource,
    pd: &Arc<ProtectionDomain>,
    recv_mr: &OwnedMemoryRegion,
    mw_rkey: u32,
    token_recv_mr: &OwnedMemoryRegion,
    deadline: tokio::time::Instant,
    ring_capacity: usize,
) -> crate::Result<(u64, u32, usize)> {
    // Send our token, then await + status-check the peer's (shared post+await).
    let our_token = RingToken {
        version: RING_TOKEN_VERSION,
        _reserved: [0; 3],
        ring_va: recv_mr.addr(),
        mw_rkey,
        capacity: ring_capacity as u32,
    };
    exchange_setup_token(qp, recv_src, pd, &our_token.to_bytes(), deadline).await?;

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

/// Reap the setup **send** completions of a ring transport so the data path
/// starts on a clean send stream: await **exactly** the `expected` setup WR ids
/// (the caller supplies them — the token Send, plus the MW binds unless MR-rkey
/// fallback) via [`CompletionSource::acquire`], under the setup `deadline`,
/// validating each completion's status and rejecting any duplicate or unknown
/// wr_id.
///
/// Replaces the previous [`CompletionSource::drain_setup`] spin, which stopped
/// after the first empty poll once *any* completion had arrived and checked
/// neither the expected count nor WC status/identity — so a late token/MW
/// completion could leak into the data path and be misread as a data completion.
pub(crate) async fn drain_ring_setup_sends(
    send_src: &mut crate::completion_source::CompletionSource,
    expected: &[u64],
    deadline: tokio::time::Instant,
) -> crate::Result<()> {
    let mut seen: u32 = 0;
    let mut remaining = expected.len();
    let mut wc = [WorkCompletion::default(); 8];

    while remaining > 0 {
        let n = match tokio::time::timeout_at(deadline, send_src.acquire(&mut wc)).await {
            Ok(r) => r?,
            Err(_) => return Err(crate::Error::Timeout("ring setup send completions")),
        };
        for w in &wc[..n] {
            let wr_id = w.wr_id();
            let Some(idx) = expected.iter().position(|&e| e == wr_id) else {
                return Err(crate::Error::ConnectionFault(
                    "ring setup: unexpected send completion wr_id",
                ));
            };
            if seen & (1u32 << idx) != 0 {
                return Err(crate::Error::ConnectionFault(
                    "ring setup: duplicate send completion wr_id",
                ));
            }
            if !w.is_success() {
                return Err(crate::Error::WorkCompletion {
                    status: w.status_raw(),
                    vendor_err: w.vendor_err(),
                });
            }
            seen |= 1u32 << idx;
            remaining -= 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared CM-disconnect / teardown / doorbell / immediate-data helpers
// (identical across read-ring, credit-ring, send-recv)
// ---------------------------------------------------------------------------

/// Drain one CM event non-blocking, setting `*peer_disconnected` on a
/// `Disconnected` event or any error. Returns whether the peer is now
/// disconnected. Shared by every transport's `check_cm_event`.
pub(crate) fn check_cm_event(event_channel: &EventChannel, peer_disconnected: &mut bool) -> bool {
    match event_channel.try_get_event() {
        Ok(ev) => {
            let etype = ev.event_type();
            ev.ack();
            if etype == CmEventType::Disconnected {
                *peer_disconnected = true;
            }
            *peer_disconnected
        }
        Err(crate::Error::WouldBlock) => false,
        Err(_) => {
            *peer_disconnected = true;
            true
        }
    }
}

/// The `Transport::poll_disconnect` loop shared by every transport: poll the CM
/// fd for readiness and drain events via [`check_cm_event`], returning `true`
/// once the peer has disconnected.
pub(crate) fn poll_cm_disconnect(
    cm_async_fd: &AsyncFd<RawFd>,
    event_channel: &EventChannel,
    peer_disconnected: &mut bool,
    cx: &mut Context<'_>,
) -> bool {
    if *peer_disconnected {
        return true;
    }
    loop {
        match cm_async_fd.poll_read_ready(cx) {
            Poll::Ready(Ok(mut guard)) => {
                guard.clear_ready();
                if check_cm_event(event_channel, peer_disconnected) {
                    return true;
                }
            }
            Poll::Pending => return false,
            Poll::Ready(Err(_)) => {
                *peer_disconnected = true;
                return true;
            }
        }
    }
}

/// The bounded teardown drain shared by the transport `Drop`s (and the read-ring
/// arm-park setup rollback): after the QP is forced to `ERROR`, reap the flushed
/// CQEs through both sources until they are empty (or the [`TEARDOWN_DRAIN_POLLS`]
/// budget is exhausted), so the QP releases its MR references before the MRs are
/// deregistered. (`read_ring::drain_teardown` uses an exact-count variant.)
pub(crate) fn drain_flushed(send_src: &mut CompletionSource, recv_src: &mut CompletionSource) {
    let mut wc = [WorkCompletion::default(); 16];
    for _ in 0..TEARDOWN_DRAIN_POLLS {
        let s = send_src.try_drain(&mut wc).unwrap_or(0);
        let r = recv_src.try_drain(&mut wc).unwrap_or(0);
        if s == 0 && r == 0 {
            break;
        }
    }
}

/// Repost a 4-byte doorbell recv WR round-robin over `bufs`, advancing
/// `*repost_idx`. Shared by both ring transports.
pub(crate) fn repost_doorbell(
    qp: &AsyncQp,
    bufs: &[OwnedMemoryRegion],
    repost_idx: &mut usize,
) -> crate::Result<()> {
    let idx = *repost_idx;
    *repost_idx = (idx + 1) % bufs.len();
    let mr = &bufs[idx];
    let sge = Sge::new(mr.addr(), 4, mr.lkey());
    let mut wr = RecvWr::new(idx as u64).sg(sge);
    qp.post_recv_wr(&mut wr)
}

/// Pack a message's `(offset, len)` into the 32-bit RDMA-Write immediate:
/// `offset` in the high 16 bits, `len` in the low 16 bits. `len == 0` signals a
/// padding (ring-wrap) message. This encoding is what hard-caps the ring at
/// 65536 bytes (16-bit fields). Shared by both ring transports' `send_gather`.
#[inline]
pub(crate) fn encode_ring_imm(offset: usize, len: usize) -> u32 {
    ((offset as u32) << 16) | (len as u32)
}

/// Inverse of [`encode_ring_imm`]: `(offset, len)` from a Write immediate.
#[inline]
pub(crate) fn decode_ring_imm(imm: u32) -> (usize, usize) {
    ((imm >> 16) as usize, (imm & 0xFFFF) as usize)
}

// ---------------------------------------------------------------------------
// CmSetupEndpoint — client/server CM endpoint abstraction for ring setup
// ---------------------------------------------------------------------------

/// A CM endpoint that can host a QP during ring-transport setup — either a
/// client [`AsyncCmId`] (post address/route resolution) or a server-side
/// [`CmId`] (from `AsyncCmListener::get_request`). Lets each transport's
/// `prepare_pending` build the QP and resource bundle once for both roles; the
/// two `connect`/`accept` paths then differ only in the role-specific CM
/// handshake. Shared by the read-ring and credit-ring setup transactions.
pub(crate) trait CmSetupEndpoint {
    fn verbs_context(&self) -> Option<Arc<crate::device::Context>>;
    fn alloc_pd(&self) -> crate::Result<Arc<ProtectionDomain>>;
    fn create_qp_with_cq(
        &self,
        pd: &Arc<ProtectionDomain>,
        init_attr: &QpInitAttr,
        send_cq: Option<&Arc<CompletionQueue>>,
        recv_cq: Option<&Arc<CompletionQueue>>,
    ) -> crate::Result<CmQueuePair>;
}

impl CmSetupEndpoint for AsyncCmId {
    fn verbs_context(&self) -> Option<Arc<crate::device::Context>> {
        AsyncCmId::verbs_context(self)
    }
    fn alloc_pd(&self) -> crate::Result<Arc<ProtectionDomain>> {
        AsyncCmId::alloc_pd(self)
    }
    fn create_qp_with_cq(
        &self,
        pd: &Arc<ProtectionDomain>,
        init_attr: &QpInitAttr,
        send_cq: Option<&Arc<CompletionQueue>>,
        recv_cq: Option<&Arc<CompletionQueue>>,
    ) -> crate::Result<CmQueuePair> {
        AsyncCmId::create_qp_with_cq(self, pd, init_attr, send_cq, recv_cq)
    }
}

impl CmSetupEndpoint for CmId {
    fn verbs_context(&self) -> Option<Arc<crate::device::Context>> {
        CmId::verbs_context(self)
    }
    fn alloc_pd(&self) -> crate::Result<Arc<ProtectionDomain>> {
        CmId::alloc_pd(self)
    }
    fn create_qp_with_cq(
        &self,
        pd: &Arc<ProtectionDomain>,
        init_attr: &QpInitAttr,
        send_cq: Option<&Arc<CompletionQueue>>,
        recv_cq: Option<&Arc<CompletionQueue>>,
    ) -> crate::Result<CmQueuePair> {
        CmId::create_qp_with_cq(self, pd, init_attr, send_cq, recv_cq)
    }
}

#[cfg(test)]
mod tests {
    use super::clamp_max_outstanding;

    // Default derivation: ring_capacity / max_message_size, device unbounded.
    #[test]
    fn derives_slot_count_from_ring_and_message_size() {
        // 64 KiB ring, 1500 B messages -> 43 slots.
        assert_eq!(clamp_max_outstanding(65536, 1500, None, 3, usize::MAX), 43);
    }

    // The undercount the fix targets: small messages get far fewer slots than
    // the ring could actually hold, until max_in_flight overrides it.
    #[test]
    fn explicit_in_flight_overrides_message_size_derivation() {
        // Without override, 64 B messages derive 1024 slots...
        assert_eq!(clamp_max_outstanding(65536, 64, None, 3, usize::MAX), 1024);
        // ...but an explicit budget takes precedence over the derived value.
        assert_eq!(
            clamp_max_outstanding(65536, 1500, Some(256), 3, usize::MAX),
            256
        );
    }

    // Result is clamped to the device queue limit minus reserve headroom.
    #[test]
    fn clamps_to_device_limit_minus_reserve() {
        // reserve = headroom.max(2) = 3; device_limit 100 -> cap 97.
        assert_eq!(clamp_max_outstanding(65536, 1, Some(10_000), 3, 100), 97);
    }

    // reserve floors at 2 even when headroom is smaller.
    #[test]
    fn reserve_floors_at_two() {
        // headroom 0 -> reserve 2; device_limit 50 -> cap 48.
        assert_eq!(clamp_max_outstanding(65536, 1, Some(10_000), 0, 50), 48);
    }

    // Always at least 1, even with degenerate inputs.
    #[test]
    fn never_below_one() {
        // Zero max_message_size must not divide-by-zero; tiny device limit still
        // yields at least 1 slot.
        assert_eq!(clamp_max_outstanding(65536, 0, None, 3, 1), 1);
        assert_eq!(clamp_max_outstanding(0, 1500, None, 3, usize::MAX), 1);
        assert_eq!(
            clamp_max_outstanding(65536, 1500, Some(0), 3, usize::MAX),
            1
        );
    }
}
