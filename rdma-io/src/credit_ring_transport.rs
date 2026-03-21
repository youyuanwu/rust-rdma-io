//! CreditRingTransport — ring-buffer transport using RDMA Write + Immediate Data.
//!
//! Implements the [`Transport`](crate::transport::Transport) trait using one-sided
//! RDMA Write with Immediate Data and ring buffers, providing zero-copy receive
//! for datagram workloads (e.g., QUIC via Quinn).
//!
//! # Credit-Based Flow Control
//!
//! The sender tracks `remote_credits` (initialized to `max_outstanding` messages).
//! Each `send_copy` decrements credits. When credits reach 0, the sender cannot
//! send until the receiver frees ring space via `repost_recv`.
//!
//! Credit updates flow from receiver to sender as `Send+Imm` messages carrying
//! the **absolute** total freed count (not a delta). This is self-healing:
//! if a credit message is lost, the next one recovers all lost credits.
//!
//! When `send_copy` finds credits exhausted, it performs a non-blocking drain
//! of the recv CQ (`drain_recv_credits`) to pick up any credit updates that
//! have already arrived. This makes the transport compatible with
//! `AsyncRdmaStream::poll_write` without requiring stream-layer changes.
//!
//! # Connection Setup
//!
//! Created via [`connect`](CreditRingTransport::connect) (client) or
//! [`accept`](CreditRingTransport::accept) (server). Both perform:
//! 1. RDMA CM connection with dual CQs
//! 2. Ring buffer + doorbell allocation
//! 3. Symmetric token exchange (20-byte `RingToken` with ring VA, rkey, capacity)
//!
//! # Limitations
//!
//! - **InfiniBand/RoCE only** — iWARP doesn't support RDMA Write with Immediate Data
//! - **MW scoping deferred** — rxe doesn't support `IBV_WR_BIND_MW` Type 2; uses MR rkey
//! - **Small ring mode** — ring ≤ 64KB, messages ≤ 65535 bytes (16-bit encoding)

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;
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
use crate::transport::{RecvCompletion, Transport, TransportBuilder};
use crate::transport_common::*;
use crate::wc::{WcOpcode, WorkCompletion};
use crate::wr::{QpType, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

// ---------------------------------------------------------------------------
// CreditRingConfig
// ---------------------------------------------------------------------------

/// Configuration for creating an [`CreditRingTransport`].
#[derive(Debug, Clone)]
pub struct CreditRingConfig {
    /// Ring buffer capacity in bytes (each direction).
    pub ring_capacity: usize,
    /// Maximum message size in bytes.
    pub max_message_size: usize,
    /// Timeout for token exchange during setup.
    pub token_timeout: Duration,
    /// Max inline data size (0 = disabled).
    pub max_inline_data: u32,
}

impl CreditRingConfig {
    /// Configuration tuned for datagram-style workloads.
    pub fn datagram() -> Self {
        Self {
            ring_capacity: 65536,
            max_message_size: 1500,
            token_timeout: Duration::from_secs(5),
            max_inline_data: 0,
        }
    }
}

impl Default for CreditRingConfig {
    fn default() -> Self {
        Self::datagram()
    }
}

// ---------------------------------------------------------------------------
// CreditRingTransport
// ---------------------------------------------------------------------------

/// Ring-buffer RDMA transport using RDMA Write + Memory Windows.
///
/// Field ordering is critical: Rust drops fields in declaration order.
/// MW must be dropped before QP (invalidation needs a live QP).
/// QP must be dropped before ring MRs. CM resources drop last.
pub struct CreditRingTransport {
    // -- CQ poll state --
    send_cq_state: CqPollState,
    recv_cq_state: CqPollState,

    // -- Connection state --
    disconnected: bool,
    peer_disconnected: bool,

    // -- Virtual buffer index mapping --
    // -- Virtual buffer index mapping (fixed at max_outstanding) --
    virtual_idx_map: Box<[Option<(usize, usize, usize)>]>, // (offset, length, arrival_seq)
    next_virt_idx: usize,
    max_outstanding: usize,
    /// Monotonic counter: sequence number assigned to each received data message.
    recv_arrival_seq: usize,

    // -- Recv stash --
    recv_stash: VecDeque<RecvCompletion>,

    // -- Credit tracking --
    remote_credits: usize,
    remote_freed_received: u32, // Last absolute freed count received from peer (u32 wire format)
    local_freed_credits: usize,
    send_in_flight: usize,

    // -- MW (dropped BEFORE QP — invalidation needs live QP) --
    _recv_mw: Option<MemoryWindow>,

    // -- QP (owns dual CQs) --
    qp: AsyncQp,

    // -- Ring buffers --
    send_ring: RingBuffer,
    recv_ring: RingBuffer,

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
    config: CreditRingConfig,

    // -- CM resources (drop last) --
    _pd: Arc<ProtectionDomain>,
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,
}

impl CreditRingTransport {
    /// Connect to a remote RDMA endpoint (client side).
    ///
    /// Performs CM address/route resolution, allocates ring buffers and MW,
    /// exchanges ring tokens with the peer, and returns a ready transport.
    pub async fn connect(addr: &SocketAddr, config: CreditRingConfig) -> crate::Result<Self> {
        // Ring transport requires InfiniBand/RoCE — iWARP doesn't support MW Type 2.
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
        let send_cq_depth = (max_outstanding + 2) as i32;
        let recv_cq_depth = (max_outstanding + 2) as i32;

        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        let qp_attr = QpInitAttr {
            qp_type: QpType::Rc,
            max_send_wr: send_cq_depth as u32,
            max_recv_wr: recv_cq_depth as u32,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: config.max_inline_data.max(RING_TOKEN_SIZE as u32),
            sq_sig_all: true,
        };

        let cmqp =
            async_cm.create_qp_with_cq(&pd, &qp_attr, Some(send_cq.cq()), Some(recv_cq.cq()))?;

        let send_mr = pd.reg_mr_owned(vec![0u8; config.ring_capacity], AccessFlags::LOCAL_WRITE)?;
        let recv_mr = pd.reg_mr_owned(
            vec![0u8; config.ring_capacity],
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE | AccessFlags::MW_BIND,
        )?;

        let doorbell_bufs: Box<[OwnedMemoryRegion]> = (0..max_outstanding)
            .map(|_| pd.reg_mr_owned(vec![0u8; 4], AccessFlags::LOCAL_WRITE))
            .collect::<crate::Result<Vec<_>>>()?
            .into_boxed_slice();

        let qp = AsyncQp::new(cmqp, send_cq, recv_cq);

        // Post token recv FIRST (before doorbells, before connect).
        // This ensures the 20-byte token recv WR is consumed by the peer's
        // token Send, not by a 4-byte doorbell WR.
        let token_recv_mr = post_token_recv(&qp, &pd)?;

        async_cm.connect(&ConnParam::default()).await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        // QP is now RTS — bind Memory Window to scope remote write access.
        let (recv_mw, mw_rkey) = bind_recv_mw(&qp, &pd, &recv_mr, config.ring_capacity)?;

        // Complete token exchange (async — waits for CQ notification).
        let (remote_addr, remote_rkey, remote_capacity) = complete_token_exchange(
            &qp,
            &pd,
            &recv_mr,
            mw_rkey,
            &token_recv_mr,
            config.token_timeout,
            config.ring_capacity,
        )
        .await?;

        // Drain setup completions from send CQ (token send + MW bind).
        drain_send_cq(&qp)?;

        // NOW post doorbell recv WRs (after token exchange is done).
        for (i, mr) in doorbell_bufs.iter().enumerate() {
            let sge = Sge::new(mr.addr(), 4, mr.lkey());
            let mut wr = RecvWr::new(i as u64).sg(sge);
            qp.post_recv_wr(&mut wr)?;
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
            doorbell_bufs,
            remote_addr,
            remote_rkey,
            remote_capacity,
            max_outstanding,
            config,
        ))
    }
    ///
    /// Waits for an incoming connection request, allocates ring buffers and MW,
    /// exchanges ring tokens with the peer, and returns a ready transport.
    pub async fn accept(
        listener: &AsyncCmListener,
        config: CreditRingConfig,
    ) -> crate::Result<Self> {
        // Ring transport requires InfiniBand/RoCE — iWARP doesn't support MW Type 2.
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
        let send_cq_depth = (max_outstanding + 2) as i32;
        let recv_cq_depth = (max_outstanding + 2) as i32;

        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        let qp_attr = QpInitAttr {
            qp_type: QpType::Rc,
            max_send_wr: send_cq_depth as u32,
            max_recv_wr: recv_cq_depth as u32,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: config.max_inline_data.max(RING_TOKEN_SIZE as u32),
            sq_sig_all: true,
        };

        let cmqp =
            conn_id.create_qp_with_cq(&pd, &qp_attr, Some(send_cq.cq()), Some(recv_cq.cq()))?;

        let send_mr = pd.reg_mr_owned(vec![0u8; config.ring_capacity], AccessFlags::LOCAL_WRITE)?;
        let recv_mr = pd.reg_mr_owned(
            vec![0u8; config.ring_capacity],
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE | AccessFlags::MW_BIND,
        )?;

        let doorbell_bufs: Box<[OwnedMemoryRegion]> = (0..max_outstanding)
            .map(|_| pd.reg_mr_owned(vec![0u8; 4], AccessFlags::LOCAL_WRITE))
            .collect::<crate::Result<Vec<_>>>()?
            .into_boxed_slice();

        let qp = AsyncQp::new(cmqp, send_cq, recv_cq);

        // Post token recv FIRST — see connect() comment.
        let token_recv_mr = post_token_recv(&qp, &pd)?;

        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        // QP is now RTS — bind Memory Window to scope remote write access.
        let (recv_mw, mw_rkey) = bind_recv_mw(&qp, &pd, &recv_mr, config.ring_capacity)?;

        // Complete token exchange (async).
        let (remote_addr, remote_rkey, remote_capacity) = complete_token_exchange(
            &qp,
            &pd,
            &recv_mr,
            mw_rkey,
            &token_recv_mr,
            config.token_timeout,
            config.ring_capacity,
        )
        .await?;

        // Drain setup completions from send CQ (token send + MW bind).
        drain_send_cq(&qp)?;

        // Post doorbell recv WRs.
        for (i, mr) in doorbell_bufs.iter().enumerate() {
            let sge = Sge::new(mr.addr(), 4, mr.lkey());
            let mut wr = RecvWr::new(i as u64).sg(sge);
            qp.post_recv_wr(&mut wr)?;
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
            doorbell_bufs,
            remote_addr,
            remote_rkey,
            remote_capacity,
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
        doorbell_bufs: Box<[OwnedMemoryRegion]>,
        remote_addr: u64,
        remote_rkey: u32,
        remote_capacity: usize,
        max_outstanding: usize,
        config: CreditRingConfig,
    ) -> Self {
        let ring_capacity = config.ring_capacity;
        Self {
            send_cq_state: CqPollState::default(),
            recv_cq_state: CqPollState::default(),

            disconnected: false,
            peer_disconnected: false,

            virtual_idx_map: vec![None; max_outstanding].into_boxed_slice(),
            next_virt_idx: 0,
            max_outstanding,
            recv_arrival_seq: 0,

            recv_stash: VecDeque::new(),

            remote_credits: max_outstanding,
            remote_freed_received: 0,
            local_freed_credits: 0,
            send_in_flight: 0,

            _recv_mw: Some(recv_mw),

            qp,

            send_ring: RingBuffer::new(send_mr, ring_capacity),
            recv_ring: RingBuffer::new(recv_mr, ring_capacity),

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

    fn check_cm_event(&mut self) -> bool {
        match self.event_channel.try_get_event() {
            Ok(ev) => {
                let etype = ev.event_type();
                ev.ack();
                if etype == crate::cm::CmEventType::Disconnected {
                    self.peer_disconnected = true;
                }
                self.peer_disconnected
            }
            Err(crate::Error::WouldBlock) => false,
            Err(_) => {
                self.peer_disconnected = true;
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

    /// Non-blocking drain of recv CQ for credit updates only.
    /// Called from send_copy when credits are exhausted, to pick up any
    /// credit Send+Imm completions without requiring the caller to poll_recv.
    /// Data completions are stashed for the next poll_recv call.
    fn drain_recv_credits(&mut self) {
        let mut wc_buf = [WorkCompletion::default(); 8];
        // Arm + drain (one pass, non-blocking).
        if self.qp.recv_cq().cq().req_notify(false).is_err() {
            self.peer_disconnected = true;
            return;
        }
        let n = match self.qp.recv_cq().cq().poll(&mut wc_buf) {
            Ok(n) => n,
            Err(_) => {
                self.peer_disconnected = true;
                return;
            }
        };
        for wc in &wc_buf[..n] {
            if !wc.is_success() {
                self.peer_disconnected = true;
                return;
            }
            match wc.opcode() {
                WcOpcode::RecvRdmaWithImm => {
                    // Data or padding — stash for poll_recv to handle.
                    let imm = wc.imm_data();
                    let offset = (imm >> 16) as usize;
                    let length = (imm & 0xFFFF) as usize;
                    if length == 0 {
                        // Padding — advance recv_ring head, repost doorbell.
                        // Padding consumes a sender credit, so assign a tracker
                        // slot and immediately release it.
                        let _pad_len = self.recv_ring.capacity - offset;
                        if self.repost_doorbell().is_err() {
                            self.peer_disconnected = true;
                            return;
                        }
                        let seq = self.recv_arrival_seq;
                        self.recv_arrival_seq += 1;
                        let slot = seq % self.max_outstanding;
                        let contiguous = self.recv_tracker.release(slot);
                        if contiguous > 0 {
                            self.local_freed_credits += contiguous;
                            let credit_imm = self.local_freed_credits as u32;
                            let mut credit_wr = SendWr::new(
                                WR_ID_CREDIT_FLAG | (self.local_freed_credits as u64),
                                WrOpcode::SendWithImm(credit_imm),
                            )
                            .flags(SendFlags::SIGNALED);
                            if self.qp.post_send_wr(&mut credit_wr).is_err() {
                                self.peer_disconnected = true;
                                return;
                            }
                            self.send_in_flight += 1;
                        }
                    } else {
                        // Data — find a virtual idx slot and stash.
                        let mut virt_idx = self.next_virt_idx;
                        let mut found = false;
                        for _ in 0..self.max_outstanding {
                            if self.virtual_idx_map[virt_idx].is_none() {
                                found = true;
                                break;
                            }
                            virt_idx = (virt_idx + 1) % self.max_outstanding;
                        }
                        if found {
                            let seq = self.recv_arrival_seq;
                            self.recv_arrival_seq += 1;
                            self.virtual_idx_map[virt_idx] = Some((offset, length, seq));
                            self.next_virt_idx = (virt_idx + 1) % self.max_outstanding;
                            self.recv_stash.push_back(RecvCompletion {
                                buf_idx: virt_idx,
                                byte_len: length,
                            });
                        }
                    }
                }
                WcOpcode::Recv => {
                    // Credit update — process it.
                    if wc.wc_flags() & rdma_io_sys::ibverbs::IBV_WC_WITH_IMM != 0 {
                        let freed_count = wc.imm_data();
                        let last = self.remote_freed_received;
                        let delta = freed_count.wrapping_sub(last) as usize;
                        if delta > 0 && delta <= self.max_outstanding {
                            self.remote_credits += delta;
                            self.remote_freed_received = freed_count;
                            tracing::debug!(
                                freed_count,
                                delta,
                                remote_credits = self.remote_credits,
                                "drain_recv_credits: credit update"
                            );
                        }
                    }
                    if self.repost_doorbell().is_err() {
                        self.peer_disconnected = true;
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

impl Transport for CreditRingTransport {
    /// Copy data into the send ring and post an RDMA Write + Immediate Data.
    ///
    /// Returns the number of bytes accepted (up to `max_message_size`), or
    /// `Ok(0)` if the transport cannot accept data right now.
    ///
    /// # Credit Exhaustion Behavior
    ///
    /// Ring transport uses explicit credit-based flow control. Each send
    /// consumes one credit; credits are restored when the peer calls
    /// `repost_recv` (which sends a credit update back).
    ///
    /// When credits are exhausted, `send_copy` performs a **non-blocking
    /// inline drain** of the recv CQ to pick up any credit updates that
    /// have already arrived. If credits are recovered, the send proceeds
    /// immediately. If not, returns `Ok(0)`.
    ///
    /// This inline drain makes ring transport compatible with
    /// `AsyncRdmaStream::poll_write`, which only calls `poll_send_completion`
    /// (not `poll_recv`) on backpressure. Without it, the writer would
    /// never see credit updates and would deadlock.
    ///
    /// Any data completions encountered during the inline drain are stashed
    /// and returned by the next `poll_recv` call.
    fn send_copy(&mut self, data: &[u8]) -> crate::Result<usize> {
        if self.peer_disconnected {
            return Err(crate::Error::WorkCompletion {
                status: rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR,
                vendor_err: 0,
            });
        }
        if data.is_empty() {
            return Ok(0);
        }
        if self.remote_credits == 0 {
            // Try to pick up credit updates from the recv CQ before giving up.
            // This is a non-blocking drain — no waker, no async, just poll the CQ
            // for any credit Send+Imm completions that have already arrived.
            self.drain_recv_credits();
            if self.remote_credits == 0 {
                return Ok(0);
            }
        }

        // Clamp data length (16-bit immediate encoding limit).
        let data_len = data
            .len()
            .min(self.config.max_message_size)
            .min(self.remote_capacity)
            .min(0xFFFF); // 16-bit length field in immediate data

        // Reserve space in send_ring.
        let (local_offset, padding) = match self.send_ring.reserve(data_len) {
            Some(result) => result,
            None => return Ok(0),
        };

        // If wrapping needed, check credits for BOTH padding + data upfront.
        if padding > 0 {
            if self.remote_credits < 2 {
                // Not enough credits for padding + data. Roll back reserve.
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
            self.remote_credits -= 1;
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
        // MSB=0 (data WR marker).
        let release_len = padding + data_len;
        let mut wr = SendWr::new(release_len as u64, WrOpcode::RdmaWriteWithImm(imm))
            .flags(SendFlags::SIGNALED)
            .sg(sge)
            .rdma(self.remote_addr + remote_offset as u64, self.remote_rkey);
        self.qp.post_send_wr(&mut wr)?;

        self.remote_write_tail = (remote_offset + data_len) % self.remote_capacity;
        self.remote_credits -= 1;
        self.send_in_flight += 1;

        tracing::debug!(
            data_len,
            remote_offset,
            remote_credits = self.remote_credits,
            send_in_flight = self.send_in_flight,
            "send_copy: posted Write+Imm"
        );

        Ok(data_len)
    }

    fn poll_send_completion(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        let mut wc_buf = [WorkCompletion::default(); 8];
        let n = match self
            .qp
            .poll_send_cq(cx, &mut self.send_cq_state, &mut wc_buf)
        {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => n,
        };
        for wc in &wc_buf[..n] {
            if !wc.is_success() {
                self.peer_disconnected = true;
                return Poll::Ready(Err(crate::Error::WorkCompletion {
                    status: wc.status_raw(),
                    vendor_err: wc.vendor_err(),
                }));
            }
            let wr_id = wc.wr_id();
            if wr_id & WR_ID_CREDIT_FLAG != 0 {
                // Credit WR — no ring space to free.
            } else if wr_id == WR_ID_PADDING_SENTINEL {
                // Padding WR — no local ring space to free (padding was at ring end).
            } else {
                // Data WR — wr_id lower bits encode data_len. Release send_ring.
                // For RC QPs, completions arrive in-order, so simple head advance works.
                let data_len = wr_id as usize;
                if data_len > 0 && data_len <= self.send_ring.capacity {
                    self.send_ring.release(data_len);
                }
            }
            self.send_in_flight = self.send_in_flight.saturating_sub(1);
        }
        Poll::Ready(Ok(()))
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

        // Poll recv CQ. Loop to handle batches that contain only credit/padding
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
            let mut got_credits = false;
            for wc in &wc_buf[..n] {
                if !wc.is_success() {
                    self.peer_disconnected = true;
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
                            self.peer_disconnected = true;
                            return Poll::Ready(Err(crate::Error::InvalidArg(
                                "recv ring offset/length out of bounds".into(),
                            )));
                        }

                        if length == 0 {
                            // Padding — repost doorbell. Assign a tracker slot
                            // and immediately release (padding is auto-consumed).
                            if self.repost_doorbell().is_err() {
                                self.peer_disconnected = true;
                                return Poll::Ready(Err(crate::Error::InvalidArg(
                                    "repost_doorbell failed".into(),
                                )));
                            }
                            let seq = self.recv_arrival_seq;
                            self.recv_arrival_seq += 1;
                            let slot = seq % self.max_outstanding;
                            let contiguous = self.recv_tracker.release(slot);
                            if contiguous > 0 {
                                self.local_freed_credits += contiguous;
                                let credit_imm = self.local_freed_credits as u32;
                                let mut credit_wr = SendWr::new(
                                    WR_ID_CREDIT_FLAG | (self.local_freed_credits as u64),
                                    WrOpcode::SendWithImm(credit_imm),
                                )
                                .flags(SendFlags::SIGNALED);
                                if self.qp.post_send_wr(&mut credit_wr).is_err() {
                                    self.peer_disconnected = true;
                                    return Poll::Ready(Err(crate::Error::InvalidArg(
                                        "credit post_send failed".into(),
                                    )));
                                }
                                self.send_in_flight += 1;
                            }
                            continue;
                        }

                        // Find a free virtual buf_idx slot.
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
                            // All slots occupied — stash this and remaining completions.
                            // The slot will free when consumer calls repost_recv().
                            // For now we can't map this completion — stash as a special
                            // entry. Actually, we need a mapped slot. Log and mark dead
                            // as a safety measure (shouldn't happen under credit control).
                            self.peer_disconnected = true;
                            return Poll::Ready(Err(crate::Error::InvalidArg(
                                "recv virtual index map exhausted".into(),
                            )));
                        }

                        let seq = self.recv_arrival_seq;
                        self.recv_arrival_seq += 1;
                        self.virtual_idx_map[virt_idx] = Some((offset, length, seq));
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
                    WcOpcode::Recv => {
                        // Credit update from peer's Send+Imm.
                        if wc.wc_flags() & rdma_io_sys::ibverbs::IBV_WC_WITH_IMM != 0 {
                            let freed_count = wc.imm_data();
                            let last = self.remote_freed_received;
                            let delta = freed_count.wrapping_sub(last) as usize;
                            if delta > 0 && delta <= self.max_outstanding {
                                self.remote_credits += delta;
                                self.remote_freed_received = freed_count;
                                tracing::debug!(
                                    freed_count,
                                    delta,
                                    remote_credits = self.remote_credits,
                                    "credit update received"
                                );
                            }
                            got_credits = true;
                        }
                        if self.repost_doorbell().is_err() {
                            self.peer_disconnected = true;
                            return Poll::Ready(Err(crate::Error::InvalidArg(
                                "repost_doorbell failed".into(),
                            )));
                        }
                    }
                    _ => {
                        // Unexpected opcode — skip.
                    }
                }
            }

            if filled > 0 || got_data {
                tracing::debug!(filled, "poll_recv: returning data completions");
                return Poll::Ready(Ok(filled));
            }
            if got_credits {
                tracing::debug!(
                    remote_credits = self.remote_credits,
                    "poll_recv: processed credits only, returning Ok(0)"
                );
                return Poll::Ready(Ok(0));
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
        // Credits are only sent for contiguous head advances — this prevents
        // the sender from wrapping remote_write_tail into unreleased ring positions
        // when repost_recv is called out of order.
        // See docs/bugs/credit-ring-ooo-overwrite.md.
        let slot = arrival_seq % self.max_outstanding;
        let contiguous = self.recv_tracker.release(slot);

        if contiguous > 0 {
            self.local_freed_credits += contiguous;

            // Send credit update: absolute freed count.
            let imm = self.local_freed_credits as u32;
            let wr_id = WR_ID_CREDIT_FLAG | (self.local_freed_credits as u64);
            let mut wr = SendWr::new(wr_id, WrOpcode::SendWithImm(imm)).flags(SendFlags::SIGNALED);
            self.qp.post_send_wr(&mut wr)?;
            self.send_in_flight += 1;

            tracing::debug!(
                contiguous,
                local_freed_credits = self.local_freed_credits,
                send_in_flight = self.send_in_flight,
                "repost_recv: sent credit update (contiguous advance)"
            );
        } else {
            tracing::debug!(
                arrival_seq,
                "repost_recv: non-contiguous release, credit deferred"
            );
        }

        Ok(())
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> bool {
        if self.peer_disconnected {
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
                    return false;
                }
                Poll::Ready(Err(_)) => {
                    self.peer_disconnected = true;
                    return true;
                }
            }
        }
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

impl Drop for CreditRingTransport {
    fn drop(&mut self) {
        if !self.disconnected {
            let _ = self.cm_id.disconnect();
        }
        // Drain CQ flush completions so the QP releases MR references
        // before MRs are deregistered (field drop order: MW → QP → MRs).
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

impl TransportBuilder for CreditRingConfig {
    type Transport = CreditRingTransport;

    async fn connect(&self, addr: &SocketAddr) -> crate::Result<CreditRingTransport> {
        CreditRingTransport::connect(addr, self.clone()).await
    }

    async fn accept(&self, listener: &AsyncCmListener) -> crate::Result<CreditRingTransport> {
        CreditRingTransport::accept(listener, self.clone()).await
    }
}
