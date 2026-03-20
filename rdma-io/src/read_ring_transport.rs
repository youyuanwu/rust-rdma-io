//! ReadRingTransport — ring-buffer transport using RDMA Write + RDMA Read flow control.
//!
//! Implements the [`Transport`](crate::transport::Transport) trait using one-sided
//! RDMA Write with Immediate Data and ring buffers, with RDMA Read–based flow
//! control instead of credit push messages.
//!
//! # RDMA Read Flow Control
//!
//! Instead of the receiver pushing credit updates via `Send+Imm` (as in
//! [`CreditRingTransport`](crate::credit_ring_transport::CreditRingTransport)),
//! the sender pulls the receiver's head position by issuing an RDMA Read of a
//! 4-byte offset buffer on the receiver side.
//!
//! The receiver writes its `recv_ring.head` to a local `AtomicU32` offset buffer
//! (~1 ns) on each contiguous `repost_recv`. The sender reads this buffer via
//! RDMA Read when it detects backpressure or proactively when free space is low.
//!
//! # Dual Memory Windows
//!
//! - **MW1** (recv ring): `REMOTE_WRITE` — sender writes data here
//! - **MW2** (offset buffer): `REMOTE_READ` — sender reads head position here
//!
//! MW2 is intentionally NOT `REMOTE_WRITE` — a forged head value from a
//! compromised sender could cause the receiver's ring to be overwritten.
//!
//! # When to Use
//!
//! Prefer ReadRingTransport for high-frequency messaging (>5M msg/sec),
//! low-utilization rings (<30%), or CPU-sensitive workloads. Prefer
//! CreditRingTransport for congested rings (>70%), backpressure-heavy,
//! or fan-in patterns.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::unix::AsyncFd;

use crate::async_cm::{AsyncCmId, AsyncCmListener};
use crate::async_cq::{AsyncCq, CqPollState};
use crate::async_qp::AsyncQp;
use crate::cm::{CmId, ConnParam, EventChannel, PortSpace};
use crate::mr::{AccessFlags, OwnedMemoryRegion};
use crate::mw::MemoryWindow;
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;
use crate::ring_common::*;
use crate::transport::{RecvCompletion, Transport, TransportBuilder};
use crate::wc::{WcOpcode, WorkCompletion};
use crate::wr::{QpType, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const READ_RING_TOKEN_VERSION: u8 = 2;
const READ_RING_TOKEN_SIZE: usize = 32;

/// Sentinel wr_id for RDMA Read of offset buffer — not a data WR.
const WR_ID_READ_SENTINEL: u64 = u64::MAX - 30;

// ---------------------------------------------------------------------------
// ReadRingToken — 32-byte connection setup token (v2)
// ---------------------------------------------------------------------------

#[repr(C, packed)]
struct ReadRingToken {
    version: u8,
    _reserved: [u8; 3],
    ring_va: u64,
    ring_capacity: u32,
    ring_rkey: u32,
    offset_va: u64,
    offset_rkey: u32,
}

const _: () = assert!(std::mem::size_of::<ReadRingToken>() == READ_RING_TOKEN_SIZE);

impl ReadRingToken {
    fn to_bytes(&self) -> [u8; READ_RING_TOKEN_SIZE] {
        let mut buf = [0u8; READ_RING_TOKEN_SIZE];
        buf[0] = self.version;
        // bytes 1..4 reserved (zero)
        buf[4..12].copy_from_slice(&self.ring_va.to_le_bytes());
        buf[12..16].copy_from_slice(&self.ring_capacity.to_le_bytes());
        buf[16..20].copy_from_slice(&self.ring_rkey.to_le_bytes());
        buf[20..28].copy_from_slice(&self.offset_va.to_le_bytes());
        buf[28..32].copy_from_slice(&self.offset_rkey.to_le_bytes());
        buf
    }

    fn from_bytes(buf: &[u8; READ_RING_TOKEN_SIZE]) -> Self {
        Self {
            version: buf[0],
            _reserved: [buf[1], buf[2], buf[3]],
            ring_va: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
            ring_capacity: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            ring_rkey: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            offset_va: u64::from_le_bytes(buf[20..28].try_into().unwrap()),
            offset_rkey: u32::from_le_bytes(buf[28..32].try_into().unwrap()),
        }
    }
}

// ---------------------------------------------------------------------------
// ReadRingConfig
// ---------------------------------------------------------------------------

/// Configuration for creating a [`ReadRingTransport`].
#[derive(Debug, Clone)]
pub struct ReadRingConfig {
    /// Ring buffer capacity in bytes (each direction).
    pub ring_capacity: usize,
    /// Maximum message size in bytes.
    pub max_message_size: usize,
    /// Timeout for token exchange during setup.
    pub token_timeout: Duration,
    /// Max inline data size (0 = disabled).
    pub max_inline_data: u32,
    /// Minimum free bytes in remote ring before triggering backpressure.
    pub min_free_threshold: usize,
}

impl ReadRingConfig {
    /// Configuration tuned for datagram-style workloads.
    pub fn datagram() -> Self {
        Self {
            ring_capacity: 65536,
            max_message_size: 1500,
            token_timeout: Duration::from_secs(5),
            max_inline_data: 0,
            min_free_threshold: 128,
        }
    }
}

impl Default for ReadRingConfig {
    fn default() -> Self {
        Self::datagram()
    }
}

// ---------------------------------------------------------------------------
// ReadRingTransport
// ---------------------------------------------------------------------------

/// Ring-buffer RDMA transport using RDMA Read–based flow control.
///
/// Field ordering is critical: Rust drops fields in declaration order.
/// MW2 → MW1 → QP → MRs → PD.
pub struct ReadRingTransport {
    // -- CQ poll state --
    send_cq_state: CqPollState,
    recv_cq_state: CqPollState,

    // -- Connection state --
    disconnected: bool,
    peer_disconnected: bool,
    qp_dead: bool,
    qp_check_counter: u32,

    // -- Virtual buffer index mapping (fixed at max_outstanding) --
    virtual_idx_map: Box<[Option<(usize, usize, usize)>]>, // (offset, length, arrival_seq)
    next_virt_idx: usize,
    max_outstanding: usize,
    /// Monotonic counter: sequence number assigned to each received data/padding message.
    recv_arrival_seq: usize,

    // -- Recv stash --
    recv_stash: VecDeque<RecvCompletion>,

    // -- Read-ring flow control (replaces credit tracking) --
    /// Sender's cached view of the remote recv ring head position.
    cached_remote_head: usize,
    /// Is an RDMA Read of the offset buffer pending?
    read_in_flight: bool,
    /// Pointer to local 4-byte offset buffer (written by receiver on repost_recv).
    offset_buffer: *mut AtomicU32,
    /// VA of the remote peer's offset buffer.
    remote_offset_va: u64,
    /// MW2 rkey for RDMA Read of remote offset buffer.
    remote_offset_rkey: u32,
    /// Per-arrival-seq-slot byte lengths for variable-size chase-forward.
    slot_lengths: Box<[usize]>,
    /// Tracks which slot_lengths entry corresponds to recv_ring.head.
    head_slot_idx: usize,

    // -- Send tracking --
    send_in_flight: usize,

    // -- MW2 (offset buffer, REMOTE_READ) — dropped BEFORE MW1 --
    _offset_mw: Option<MemoryWindow>,
    // -- MW1 (recv ring, REMOTE_WRITE) — dropped BEFORE QP --
    _recv_mw: Option<MemoryWindow>,

    // -- QP (owns dual CQs) --
    qp: AsyncQp,

    // -- Ring buffers --
    send_ring: RingBuffer,
    recv_ring: RingBuffer,

    // -- RDMA Read landing buffer (4 bytes, LOCAL_WRITE) --
    read_buf: OwnedMemoryRegion,

    // -- Offset buffer MR (64 bytes, LOCAL_WRITE | REMOTE_READ | MW_BIND) --
    _offset_mr: OwnedMemoryRegion,

    // -- Remote ring info --
    remote_addr: u64,
    remote_rkey: u32,
    remote_capacity: usize,
    /// Position in the remote ring where the next write goes.
    remote_write_tail: usize,

    // -- Doorbell recv buffers (fixed at max_outstanding) --
    doorbell_bufs: Box<[OwnedMemoryRegion]>,
    /// Round-robin index for reposting doorbell recv buffers.
    doorbell_repost_idx: usize,

    // -- Completion tracker (recv side, for OOO repost_recv) --
    recv_tracker: CompletionTracker,

    // -- Config --
    config: ReadRingConfig,

    // -- CM resources (drop last) --
    _pd: Arc<ProtectionDomain>,
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,
}

// Safety: Raw pointer `offset_buffer` points into `offset_mr` memory which is
// pinned by ibverbs registration and valid for the struct's lifetime. All RDMA
// kernel-managed handles (*mut ibv_mr, *mut ibv_mw etc.) are safe to send
// between threads — same justification as OwnedMemoryRegion.
unsafe impl Send for ReadRingTransport {}
unsafe impl Sync for ReadRingTransport {}

// ---------------------------------------------------------------------------
// Token exchange helpers (32-byte ReadRingToken, different from ring_common)
// ---------------------------------------------------------------------------

/// Post a 32-byte recv WR for the peer's ReadRingToken.
fn post_read_ring_token_recv(
    qp: &AsyncQp,
    pd: &Arc<ProtectionDomain>,
) -> crate::Result<OwnedMemoryRegion> {
    let token_recv_mr =
        pd.reg_mr_owned(vec![0u8; READ_RING_TOKEN_SIZE], AccessFlags::LOCAL_WRITE)?;
    let recv_sge = Sge::new(
        token_recv_mr.addr(),
        READ_RING_TOKEN_SIZE as u32,
        token_recv_mr.lkey(),
    );
    let mut recv_wr = RecvWr::new(u64::MAX).sg(recv_sge);
    qp.post_recv_wr(&mut recv_wr)?;
    Ok(token_recv_mr)
}

/// Send our 32-byte token and wait for the peer's token.
async fn complete_read_ring_token_exchange(
    qp: &AsyncQp,
    pd: &Arc<ProtectionDomain>,
    our_token: &ReadRingToken,
    token_recv_mr: &OwnedMemoryRegion,
) -> crate::Result<ReadRingToken> {
    let token_bytes = our_token.to_bytes();
    let token_send_mr = pd.reg_mr_owned(token_bytes.to_vec(), AccessFlags::LOCAL_WRITE)?;
    let send_sge = Sge::new(
        token_send_mr.addr(),
        READ_RING_TOKEN_SIZE as u32,
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

    let recv_buf: &[u8; READ_RING_TOKEN_SIZE] = token_recv_mr
        .as_slice()
        .try_into()
        .expect("token recv MR is exactly READ_RING_TOKEN_SIZE");
    let peer_token = ReadRingToken::from_bytes(recv_buf);

    let peer_ver = peer_token.version;
    if peer_ver != READ_RING_TOKEN_VERSION {
        return Err(crate::Error::InvalidArg(format!(
            "unsupported read ring token version: {peer_ver}",
        )));
    }
    let peer_cap = peer_token.ring_capacity;
    if peer_cap == 0 {
        return Err(crate::Error::InvalidArg("peer ring capacity is 0".into()));
    }
    if peer_cap as usize > 65536 {
        return Err(crate::Error::InvalidArg(format!(
            "peer ring capacity too large: {peer_cap}",
        )));
    }

    Ok(peer_token)
}

/// Bind MW Type 2 to the offset buffer MR with REMOTE_READ access.
fn bind_offset_mw(
    qp: &AsyncQp,
    pd: &Arc<ProtectionDomain>,
    offset_mr: &OwnedMemoryRegion,
    offset_size: usize,
) -> crate::Result<(MemoryWindow, u32)> {
    let mw = MemoryWindow::alloc(pd, crate::mw::MwType::Type2)?;
    let mw_rkey = mw.rkey();

    let mut bind_wr = SendWr::new(u64::MAX - 11, WrOpcode::BindMw)
        .flags(SendFlags::SIGNALED)
        .bind_mw(
            mw.as_raw(),
            mw_rkey,
            offset_mr.as_raw(),
            offset_mr.addr(),
            offset_size as u64,
            rdma_io_sys::ibverbs::IBV_ACCESS_REMOTE_READ,
        );
    qp.post_send_wr(&mut bind_wr)?;

    Ok((mw, mw_rkey))
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl ReadRingTransport {
    /// Connect to a remote RDMA endpoint (client side).
    pub async fn connect(addr: &SocketAddr, config: ReadRingConfig) -> crate::Result<Self> {
        if crate::device::any_device_is_iwarp() {
            return Err(crate::Error::InvalidArg(
                "ring transport requires InfiniBand/RoCE (iWARP detected)".into(),
            ));
        }

        let async_cm = AsyncCmId::new(PortSpace::Tcp)?;
        async_cm.resolve_addr(None, addr, 2000).await?;
        async_cm.resolve_route(2000).await?;

        let ctx = async_cm
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = async_cm.alloc_pd()?;

        let max_outstanding = config.ring_capacity / config.max_message_size;
        // +3: headroom for RDMA Read WR on send CQ
        let send_cq_depth = (max_outstanding + 3) as i32;
        let recv_cq_depth = (max_outstanding + 2) as i32;

        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        let qp_attr = QpInitAttr {
            qp_type: QpType::Rc,
            max_send_wr: send_cq_depth as u32,
            max_recv_wr: recv_cq_depth as u32,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: config.max_inline_data.max(READ_RING_TOKEN_SIZE as u32),
            sq_sig_all: true,
        };

        let cmqp =
            async_cm.create_qp_with_cq(&pd, &qp_attr, Some(send_cq.cq()), Some(recv_cq.cq()))?;

        // Allocate ring MRs (separate, like CreditRing).
        let send_mr = pd.reg_mr_owned(vec![0u8; config.ring_capacity], AccessFlags::LOCAL_WRITE)?;
        let recv_mr = pd.reg_mr_owned(
            vec![0u8; config.ring_capacity],
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE | AccessFlags::MW_BIND,
        )?;

        // Offset buffer: 64 bytes (cache-line aligned allocation).
        let offset_mr = pd.reg_mr_owned(
            vec![0u8; 64],
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ | AccessFlags::MW_BIND,
        )?;

        // RDMA Read landing buffer: 4 bytes.
        let read_buf = pd.reg_mr_owned(vec![0u8; 4], AccessFlags::LOCAL_WRITE)?;

        let doorbell_bufs: Box<[OwnedMemoryRegion]> = (0..max_outstanding)
            .map(|_| pd.reg_mr_owned(vec![0u8; 4], AccessFlags::LOCAL_WRITE))
            .collect::<crate::Result<Vec<_>>>()?
            .into_boxed_slice();

        let qp = AsyncQp::new(cmqp, send_cq, recv_cq);

        // Post 32-byte token recv FIRST (before doorbells, before connect).
        let token_recv_mr = post_read_ring_token_recv(&qp, &pd)?;

        async_cm.connect(&ConnParam::default()).await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        // QP is now RTS — bind Memory Windows.
        let (recv_mw, mw1_rkey) = bind_recv_mw(&qp, &pd, &recv_mr, config.ring_capacity)?;
        let (offset_mw, mw2_rkey) = bind_offset_mw(&qp, &pd, &offset_mr, 64)?;

        // Build our token.
        let our_token = ReadRingToken {
            version: READ_RING_TOKEN_VERSION,
            _reserved: [0; 3],
            ring_va: recv_mr.addr(),
            ring_capacity: config.ring_capacity as u32,
            ring_rkey: mw1_rkey,
            offset_va: offset_mr.addr(),
            offset_rkey: mw2_rkey,
        };

        // Complete token exchange (async — waits for CQ notification).
        let peer_token =
            complete_read_ring_token_exchange(&qp, &pd, &our_token, &token_recv_mr).await?;

        // Drain setup completions from send CQ (token send + 2 MW binds).
        drain_send_cq(&qp);

        // Post doorbell recv WRs.
        for (i, mr) in doorbell_bufs.iter().enumerate() {
            let sge = Sge::new(mr.addr(), 4, mr.lkey());
            let mut wr = RecvWr::new(i as u64).sg(sge);
            qp.post_recv_wr(&mut wr)?;
        }

        // Derive offset_buffer pointer from MR.
        let offset_ptr = offset_mr.addr() as *mut AtomicU32;

        // Initialize offset buffer to 0 (already zeroed, but explicit).
        unsafe {
            (*offset_ptr).store(0, Ordering::Release);
        }

        Ok(Self::from_parts(
            qp,
            cm_async_fd,
            cm_id,
            event_channel,
            pd,
            send_mr,
            recv_mr,
            recv_mw,
            offset_mw,
            offset_mr,
            offset_ptr,
            read_buf,
            doorbell_bufs,
            peer_token.ring_va,
            peer_token.ring_rkey,
            peer_token.ring_capacity as usize,
            peer_token.offset_va,
            peer_token.offset_rkey,
            max_outstanding,
            config,
        ))
    }

    /// Accept an incoming connection (server side).
    pub async fn accept(listener: &AsyncCmListener, config: ReadRingConfig) -> crate::Result<Self> {
        if crate::device::any_device_is_iwarp() {
            return Err(crate::Error::InvalidArg(
                "ring transport requires InfiniBand/RoCE (iWARP detected)".into(),
            ));
        }

        let conn_id = listener.get_request().await?;

        let ctx = conn_id
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = conn_id.alloc_pd()?;

        let max_outstanding = config.ring_capacity / config.max_message_size;
        let send_cq_depth = (max_outstanding + 3) as i32;
        let recv_cq_depth = (max_outstanding + 2) as i32;

        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        let qp_attr = QpInitAttr {
            qp_type: QpType::Rc,
            max_send_wr: send_cq_depth as u32,
            max_recv_wr: recv_cq_depth as u32,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: config.max_inline_data.max(READ_RING_TOKEN_SIZE as u32),
            sq_sig_all: true,
        };

        let cmqp =
            conn_id.create_qp_with_cq(&pd, &qp_attr, Some(send_cq.cq()), Some(recv_cq.cq()))?;

        let send_mr = pd.reg_mr_owned(vec![0u8; config.ring_capacity], AccessFlags::LOCAL_WRITE)?;
        let recv_mr = pd.reg_mr_owned(
            vec![0u8; config.ring_capacity],
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE | AccessFlags::MW_BIND,
        )?;

        let offset_mr = pd.reg_mr_owned(
            vec![0u8; 64],
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ | AccessFlags::MW_BIND,
        )?;

        let read_buf = pd.reg_mr_owned(vec![0u8; 4], AccessFlags::LOCAL_WRITE)?;

        let doorbell_bufs: Box<[OwnedMemoryRegion]> = (0..max_outstanding)
            .map(|_| pd.reg_mr_owned(vec![0u8; 4], AccessFlags::LOCAL_WRITE))
            .collect::<crate::Result<Vec<_>>>()?
            .into_boxed_slice();

        let qp = AsyncQp::new(cmqp, send_cq, recv_cq);

        // Post 32-byte token recv FIRST.
        let token_recv_mr = post_read_ring_token_recv(&qp, &pd)?;

        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        // QP is now RTS — bind Memory Windows.
        let (recv_mw, mw1_rkey) = bind_recv_mw(&qp, &pd, &recv_mr, config.ring_capacity)?;
        let (offset_mw, mw2_rkey) = bind_offset_mw(&qp, &pd, &offset_mr, 64)?;

        let our_token = ReadRingToken {
            version: READ_RING_TOKEN_VERSION,
            _reserved: [0; 3],
            ring_va: recv_mr.addr(),
            ring_capacity: config.ring_capacity as u32,
            ring_rkey: mw1_rkey,
            offset_va: offset_mr.addr(),
            offset_rkey: mw2_rkey,
        };

        let peer_token =
            complete_read_ring_token_exchange(&qp, &pd, &our_token, &token_recv_mr).await?;

        drain_send_cq(&qp);

        for (i, mr) in doorbell_bufs.iter().enumerate() {
            let sge = Sge::new(mr.addr(), 4, mr.lkey());
            let mut wr = RecvWr::new(i as u64).sg(sge);
            qp.post_recv_wr(&mut wr)?;
        }

        let offset_ptr = offset_mr.addr() as *mut AtomicU32;
        unsafe {
            (*offset_ptr).store(0, Ordering::Release);
        }

        Ok(Self::from_parts(
            qp,
            cm_async_fd,
            cm_id,
            event_channel,
            pd,
            send_mr,
            recv_mr,
            recv_mw,
            offset_mw,
            offset_mr,
            offset_ptr,
            read_buf,
            doorbell_bufs,
            peer_token.ring_va,
            peer_token.ring_rkey,
            peer_token.ring_capacity as usize,
            peer_token.offset_va,
            peer_token.offset_rkey,
            max_outstanding,
            config,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        qp: AsyncQp,
        cm_async_fd: AsyncFd<RawFd>,
        cm_id: CmId,
        event_channel: EventChannel,
        pd: Arc<ProtectionDomain>,
        send_mr: OwnedMemoryRegion,
        recv_mr: OwnedMemoryRegion,
        recv_mw: MemoryWindow,
        offset_mw: MemoryWindow,
        offset_mr: OwnedMemoryRegion,
        offset_ptr: *mut AtomicU32,
        read_buf: OwnedMemoryRegion,
        doorbell_bufs: Box<[OwnedMemoryRegion]>,
        remote_addr: u64,
        remote_rkey: u32,
        remote_capacity: usize,
        remote_offset_va: u64,
        remote_offset_rkey: u32,
        max_outstanding: usize,
        config: ReadRingConfig,
    ) -> Self {
        let ring_capacity = config.ring_capacity;
        Self {
            send_cq_state: CqPollState::default(),
            recv_cq_state: CqPollState::default(),

            disconnected: false,
            peer_disconnected: false,
            qp_dead: false,
            qp_check_counter: 0,

            virtual_idx_map: vec![None; max_outstanding].into_boxed_slice(),
            next_virt_idx: 0,
            max_outstanding,
            recv_arrival_seq: 0,

            recv_stash: VecDeque::new(),

            cached_remote_head: 0,
            read_in_flight: false,
            offset_buffer: offset_ptr,
            remote_offset_va,
            remote_offset_rkey,
            slot_lengths: vec![0usize; max_outstanding].into_boxed_slice(),
            head_slot_idx: 0,

            send_in_flight: 0,

            _offset_mw: Some(offset_mw),
            _recv_mw: Some(recv_mw),

            qp,

            send_ring: RingBuffer::new(send_mr, ring_capacity),
            recv_ring: RingBuffer::new(recv_mr, ring_capacity),

            read_buf,
            _offset_mr: offset_mr,

            remote_addr,
            remote_rkey,
            remote_capacity,
            remote_write_tail: 0,

            doorbell_bufs,
            doorbell_repost_idx: 0,

            recv_tracker: CompletionTracker::new(max_outstanding),

            config,

            _pd: pd,
            cm_async_fd,
            cm_id,
            event_channel,
        }
    }

    /// Calculate free space in the remote recv ring.
    fn remote_free_space(&self) -> usize {
        let used = if self.remote_write_tail >= self.cached_remote_head {
            self.remote_write_tail - self.cached_remote_head
        } else {
            self.remote_capacity - self.cached_remote_head + self.remote_write_tail
        };
        self.remote_capacity.saturating_sub(used + 1)
    }

    fn check_cm_event(&mut self) -> bool {
        match self.event_channel.try_get_event() {
            Ok(ev) => {
                let etype = ev.event_type();
                ev.ack();
                if etype == crate::cm::CmEventType::Disconnected {
                    self.peer_disconnected = true;
                }
                if is_qp_dead(self.qp.as_raw()) {
                    self.qp_dead = true;
                }
                self.peer_disconnected || self.qp_dead
            }
            Err(crate::Error::WouldBlock) => {
                if is_qp_dead(self.qp.as_raw()) {
                    self.qp_dead = true;
                    return true;
                }
                false
            }
            Err(_) => {
                self.qp_dead = true;
                true
            }
        }
    }

    /// Repost a doorbell recv buffer (round-robin).
    fn repost_doorbell(&mut self) -> crate::Result<()> {
        let idx = self.doorbell_repost_idx;
        self.doorbell_repost_idx = (idx + 1) % self.doorbell_bufs.len();
        let mr = &self.doorbell_bufs[idx];
        let sge = Sge::new(mr.addr(), 4, mr.lkey());
        let mut wr = RecvWr::new(idx as u64).sg(sge);
        self.qp.post_recv_wr(&mut wr)
    }

    /// Post an RDMA Read of the remote offset buffer.
    fn post_offset_read(&mut self) -> crate::Result<()> {
        let sge = Sge::new(self.read_buf.addr(), 4, self.read_buf.lkey());
        let mut wr = SendWr::new(WR_ID_READ_SENTINEL, WrOpcode::RdmaRead)
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .rdma(self.remote_offset_va, self.remote_offset_rkey);
        self.qp.post_send_wr(&mut wr)?;
        self.read_in_flight = true;
        Ok(())
    }

    /// Read the RDMA Read landing buffer and update cached_remote_head.
    fn update_cached_remote_head(&mut self) {
        let head_val = u32::from_ne_bytes(self.read_buf.as_slice()[..4].try_into().unwrap());
        self.cached_remote_head = head_val as usize;
    }

    /// Non-blocking drain of send CQ to pick up RDMA Read completions.
    /// Also handles Write and padding completions encountered along the way.
    fn drain_send_cq_for_read(&mut self) {
        let mut wc_buf = [WorkCompletion::default(); 8];
        let _ = self.qp.send_cq().cq().req_notify(false);
        let n = match self.qp.send_cq().cq().poll(&mut wc_buf) {
            Ok(n) => n,
            Err(_) => return,
        };
        for wc in &wc_buf[..n] {
            if !wc.is_success() {
                self.qp_dead = true;
                return;
            }
            let wr_id = wc.wr_id();
            if wr_id == WR_ID_READ_SENTINEL {
                self.update_cached_remote_head();
                self.read_in_flight = false;
            } else if wr_id == WR_ID_PADDING_SENTINEL {
                self.send_in_flight = self.send_in_flight.saturating_sub(1);
            } else {
                let data_len = wr_id as usize;
                if data_len > 0 && data_len <= self.send_ring.capacity {
                    self.send_ring.release(data_len);
                }
                self.send_in_flight = self.send_in_flight.saturating_sub(1);
            }
        }
    }

    /// Advance recv_ring head by contiguous released slot lengths and update offset buffer.
    fn advance_recv_head(&mut self, contiguous: usize) {
        for _ in 0..contiguous {
            let slot_len = self.slot_lengths[self.head_slot_idx];
            self.recv_ring.release(slot_len);
            self.head_slot_idx = (self.head_slot_idx + 1) % self.max_outstanding;
        }
        // Update offset buffer (AtomicU32 for portability).
        unsafe {
            (*self.offset_buffer).store(self.recv_ring.head as u32, Ordering::Release);
        }
    }
}

impl Transport for ReadRingTransport {
    /// Copy data into the send ring and post an RDMA Write + Immediate Data.
    ///
    /// Returns the number of bytes accepted, or `Ok(0)` if the transport
    /// cannot accept data right now (remote ring full or local ring full).
    ///
    /// When the remote ring's free space drops below `min_free_threshold`,
    /// posts an RDMA Read to refresh the cached head position.
    fn send_copy(&mut self, data: &[u8]) -> crate::Result<usize> {
        if self.qp_dead {
            return Err(crate::Error::WorkCompletion {
                status: rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR,
                vendor_err: 0,
            });
        }
        if data.is_empty() {
            return Ok(0);
        }

        // Pick up completed RDMA Read if pending.
        if self.read_in_flight {
            self.drain_send_cq_for_read();
        }

        let free = self.remote_free_space();

        // Clamp data length (16-bit immediate encoding limit).
        let data_len = data
            .len()
            .min(self.config.max_message_size)
            .min(self.remote_capacity)
            .min(0xFFFF);

        if free < data_len + self.config.min_free_threshold {
            // Backpressure — post RDMA Read if not already in flight.
            if !self.read_in_flight {
                self.post_offset_read()?;
            }
            return Ok(0);
        }

        // Reserve space in local send_ring.
        let (local_offset, padding) = match self.send_ring.reserve(data_len) {
            Some(result) => result,
            None => return Ok(0),
        };

        // If wrapping needed, check remote free space for BOTH padding + data.
        if padding > 0 {
            if free < padding + data_len + self.config.min_free_threshold {
                // Not enough space. Roll back reserve.
                self.send_ring.tail = if local_offset == 0 {
                    self.send_ring.capacity - padding
                } else {
                    local_offset
                };
                return Ok(0);
            }
            let pad_remote_offset = self.remote_write_tail;
            let imm = (pad_remote_offset as u32) << 16; // length=0 signals padding
            let mut pad_wr = SendWr::new(WR_ID_PADDING_SENTINEL, WrOpcode::RdmaWriteWithImm(imm))
                .flags(SendFlags::SIGNALED)
                .rdma(
                    self.remote_addr + pad_remote_offset as u64,
                    self.remote_rkey,
                );
            self.qp.post_send_wr(&mut pad_wr)?;
            self.send_in_flight += 1;
            self.remote_write_tail = 0;
        }

        // Copy data into send_ring at reserved offset.
        self.send_ring.mr.as_mut_slice()[local_offset..local_offset + data_len]
            .copy_from_slice(&data[..data_len]);

        // Build RDMA Write+Imm WR for data.
        let remote_offset = self.remote_write_tail;
        let imm = ((remote_offset as u32) << 16) | (data_len as u32);
        let sge = Sge::new(
            self.send_ring.mr.addr() + local_offset as u64,
            data_len as u32,
            self.send_ring.mr.lkey(),
        );
        // wr_id encodes total bytes to release (padding + data) for send_ring on completion.
        let release_len = padding + data_len;
        let mut wr = SendWr::new(release_len as u64, WrOpcode::RdmaWriteWithImm(imm))
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .rdma(self.remote_addr + remote_offset as u64, self.remote_rkey);
        self.qp.post_send_wr(&mut wr)?;

        self.remote_write_tail = (remote_offset + data_len) % self.remote_capacity;
        self.send_in_flight += 1;

        // Proactive Read when remaining space is getting low.
        if !self.read_in_flight {
            let remaining = self.remote_free_space();
            if remaining < data_len * 2 + self.config.min_free_threshold {
                let _ = self.post_offset_read();
            }
        }

        Ok(data_len)
    }

    fn poll_send_completion(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        loop {
            let mut wc_buf = [WorkCompletion::default(); 8];
            let n = match self
                .qp
                .poll_send_cq(cx, &mut self.send_cq_state, &mut wc_buf)
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(n)) => n,
            };

            let mut got_write = false;
            for wc in &wc_buf[..n] {
                if !wc.is_success() {
                    self.qp_dead = true;
                    return Poll::Ready(Err(crate::Error::WorkCompletion {
                        status: wc.status_raw(),
                        vendor_err: wc.vendor_err(),
                    }));
                }
                let wr_id = wc.wr_id();
                if wr_id == WR_ID_READ_SENTINEL {
                    // RDMA Read completed — update cached head, don't count as send.
                    self.update_cached_remote_head();
                    self.read_in_flight = false;
                } else if wr_id == WR_ID_PADDING_SENTINEL {
                    // Padding WR — no local ring space to free.
                    self.send_in_flight = self.send_in_flight.saturating_sub(1);
                    got_write = true;
                } else {
                    // Data WR — release send_ring.
                    let data_len = wr_id as usize;
                    if data_len > 0 && data_len <= self.send_ring.capacity {
                        self.send_ring.release(data_len);
                    }
                    self.send_in_flight = self.send_in_flight.saturating_sub(1);
                    got_write = true;
                }
            }

            if got_write {
                return Poll::Ready(Ok(()));
            }
            // Only Read completions in this batch — loop to re-poll CQ.
        }
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut [RecvCompletion],
    ) -> Poll<crate::Result<usize>> {
        let mut filled = 0;

        // Drain recv_stash first.
        while filled < out.len() {
            if let Some(rc) = self.recv_stash.pop_front() {
                out[filled] = rc;
                filled += 1;
            } else {
                break;
            }
        }

        if filled >= out.len() {
            return Poll::Ready(Ok(filled));
        }

        // Poll recv CQ. Loop to handle batches that contain only padding
        // completions — we must re-poll the CQ rather than returning Pending
        // without a waker registration.
        loop {
            let mut wc_buf = [WorkCompletion::default(); 8];
            let n = match self
                .qp
                .poll_recv_cq(cx, &mut self.recv_cq_state, &mut wc_buf)
            {
                Poll::Pending => {
                    if filled > 0 {
                        return Poll::Ready(Ok(filled));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(n)) => n,
            };

            let mut got_data = false;
            for wc in &wc_buf[..n] {
                if !wc.is_success() {
                    self.qp_dead = true;
                    return Poll::Ready(Err(crate::Error::WorkCompletion {
                        status: wc.status_raw(),
                        vendor_err: wc.vendor_err(),
                    }));
                }

                match wc.opcode() {
                    WcOpcode::RecvRdmaWithImm => {
                        let imm = wc.imm_data();
                        let offset = (imm >> 16) as usize;
                        let length = (imm & 0xFFFF) as usize;

                        if offset >= self.recv_ring.capacity
                            || (length > 0 && offset + length > self.recv_ring.capacity)
                        {
                            self.qp_dead = true;
                            return Poll::Ready(Err(crate::Error::InvalidArg(
                                "recv ring offset/length out of bounds".into(),
                            )));
                        }

                        if length == 0 {
                            // Padding — repost doorbell, assign tracker slot,
                            // immediately release.
                            let pad_len = self.recv_ring.capacity - offset;
                            let _ = self.repost_doorbell();
                            let seq = self.recv_arrival_seq;
                            self.recv_arrival_seq += 1;
                            let slot = seq % self.max_outstanding;
                            self.slot_lengths[slot] = pad_len;
                            let contiguous = self.recv_tracker.release(slot);
                            if contiguous > 0 {
                                self.advance_recv_head(contiguous);
                            }
                            continue;
                        }

                        // Data — find a free virtual buf_idx slot.
                        let mut virt_idx = self.next_virt_idx;
                        let mut found = false;
                        for _ in 0..self.max_outstanding {
                            if self.virtual_idx_map[virt_idx].is_none() {
                                found = true;
                                break;
                            }
                            virt_idx = (virt_idx + 1) % self.max_outstanding;
                        }
                        if !found {
                            self.qp_dead = true;
                            return Poll::Ready(Err(crate::Error::InvalidArg(
                                "recv virtual index map exhausted".into(),
                            )));
                        }

                        let seq = self.recv_arrival_seq;
                        self.recv_arrival_seq += 1;
                        self.virtual_idx_map[virt_idx] = Some((offset, length, seq));
                        self.slot_lengths[seq % self.max_outstanding] = length;
                        self.next_virt_idx = (virt_idx + 1) % self.max_outstanding;

                        let rc = RecvCompletion {
                            buf_idx: virt_idx,
                            byte_len: length,
                        };

                        if filled < out.len() {
                            out[filled] = rc;
                            filled += 1;
                        } else {
                            self.recv_stash.push_back(rc);
                        }
                        got_data = true;
                    }
                    _ => {
                        // Unexpected opcode — skip.
                    }
                }
            }

            if filled > 0 || got_data {
                return Poll::Ready(Ok(filled));
            }
            // All completions were padding only — loop to re-poll CQ.
        }
    }

    fn recv_buf(&self, buf_idx: usize) -> &[u8] {
        let (offset, length, _seq) =
            self.virtual_idx_map[buf_idx].expect("recv_buf called with invalid buf_idx");
        &self.recv_ring.mr.as_slice()[offset..offset + length]
    }

    fn repost_recv(&mut self, buf_idx: usize) -> crate::Result<()> {
        let (_offset, _length, arrival_seq) = self.virtual_idx_map[buf_idx]
            .take()
            .expect("repost_recv called with invalid buf_idx");

        // Repost a doorbell recv buffer.
        self.repost_doorbell()?;

        // Mark this arrival sequence slot as released and chase forward.
        let slot = arrival_seq % self.max_outstanding;
        let contiguous = self.recv_tracker.release(slot);

        if contiguous > 0 {
            self.advance_recv_head(contiguous);
        }

        Ok(())
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> bool {
        if self.qp_dead {
            return true;
        }
        loop {
            match self.cm_async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(mut guard)) => {
                    guard.clear_ready();
                    if self.check_cm_event() {
                        return true;
                    }
                }
                Poll::Pending => {
                    // CM fd has nothing — check QP state as fallback.
                    self.qp_check_counter += 1;
                    if self.qp_check_counter >= 64 {
                        self.qp_check_counter = 0;
                        if is_qp_dead(self.qp.as_raw()) {
                            self.qp_dead = true;
                            return true;
                        }
                    }
                    return false;
                }
                Poll::Ready(Err(_)) => {
                    self.qp_dead = true;
                    return true;
                }
            }
        }
    }

    fn is_qp_dead(&self) -> bool {
        self.qp_dead
    }

    fn disconnect(&mut self) -> crate::Result<()> {
        if !self.disconnected {
            self.cm_id.disconnect()?;
            self.disconnected = true;
        }
        Ok(())
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.cm_id.local_addr()
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.cm_id.peer_addr()
    }
}

impl Drop for ReadRingTransport {
    fn drop(&mut self) {
        if !self.disconnected {
            let _ = self.cm_id.disconnect();
        }
        // Drain CQ flush completions so the QP releases MR references
        // before MRs are deregistered (field drop order: MW2 → MW1 → QP → MRs).
        let mut wc = [WorkCompletion::default(); 16];
        loop {
            match self.qp.send_cq().cq().poll(&mut wc) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        loop {
            match self.qp.recv_cq().cq().poll(&mut wc) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    }
}

impl TransportBuilder for ReadRingConfig {
    type Transport = ReadRingTransport;

    async fn connect(&self, addr: &SocketAddr) -> crate::Result<ReadRingTransport> {
        ReadRingTransport::connect(addr, self.clone()).await
    }

    async fn accept(&self, listener: &AsyncCmListener) -> crate::Result<ReadRingTransport> {
        ReadRingTransport::accept(listener, self.clone()).await
    }
}
