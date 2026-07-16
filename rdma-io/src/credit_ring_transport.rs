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
use crate::async_cq::AsyncCq;
use crate::async_qp::AsyncQp;
use crate::cm::{CmId, ConnParam, EventChannel, PortSpace};
use crate::completion_source::CompletionSource;
use crate::mr::{AccessFlags, OwnedMemoryRegion};
use crate::mw::MemoryWindow;
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;
use crate::transport::{RecvCompletion, Transport, TransportBuilder};
use crate::transport_common::*;
use crate::wc::{WcOpcode, WorkCompletion};
use crate::wr::{QpType, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

/// Extra send-queue slots beyond `max_outstanding` (headroom for in-flight
/// credit `Send` WRs). Must match the value used to size `max_send_wr`.
const CREDIT_RING_SQ_HEADROOM: usize = 2;

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
    /// Optional explicit in-flight message budget (send/doorbell/CQ slot count).
    ///
    /// `None` (default) derives it as `ring_capacity / max_message_size`, which
    /// undercounts small messages. Set `Some(n)` to size the queues for `n`
    /// in-flight messages independently of `max_message_size` — useful for deep
    /// pipelines of small messages. Clamped to the device's queue limits.
    /// See docs/bugs/ring-send-queue-exhaustion.md.
    pub max_in_flight: Option<usize>,
    /// Timeout for token exchange during setup.
    pub token_timeout: Duration,
    /// Max inline data size (0 = disabled).
    pub max_inline_data: u32,
    /// How remote-accessible memory is exposed: a Type-2 Memory Window or the
    /// MR's own rkey.
    ///
    /// Defaults to [`MemoryWindowMode::Auto`], which binds a Memory Window when
    /// the routed device supports one and automatically falls back to the MR
    /// rkey on NICs that report `max_mw = 0` (e.g. Azure MANA). The effective
    /// mode is resolved once at connect/accept time via `resolve_mr_rkey`.
    ///
    /// In the MR-rkey path the recv MR already covers exactly the ring region,
    /// so the exposed memory is the same as the MW-bound case. What is given up
    /// is Type-2 MW protection: per-bind rkey randomization, QP association, and
    /// revocability. The static MR rkey is scoped to this connection's PD (each
    /// connection allocates its own), so a leaked rkey cannot reach other
    /// connections' memory.
    pub mw_mode: MemoryWindowMode,
}

impl CreditRingConfig {
    /// Configuration tuned for datagram-style workloads.
    pub fn datagram() -> Self {
        Self {
            ring_capacity: 65536,
            max_message_size: 1500,
            max_in_flight: None,
            token_timeout: Duration::from_secs(5),
            max_inline_data: 0,
            mw_mode: MemoryWindowMode::Auto,
        }
    }

    /// Set the [`MemoryWindowMode`] (Type-2 Memory Window vs. MR-rkey).
    ///
    /// Defaults to [`MemoryWindowMode::Auto`], which falls back to the MR rkey
    /// automatically on NICs that report `max_mw = 0` (e.g. Azure MANA).
    pub fn with_memory_window_mode(mut self, mode: MemoryWindowMode) -> Self {
        self.mw_mode = mode;
        self
    }

    /// Set an explicit in-flight message budget, sizing the send/doorbell/CQ
    /// queues for `n` messages independently of `max_message_size`. Clamped to
    /// the device's queue limits. See [`CreditRingConfig::max_in_flight`].
    pub fn with_max_in_flight(mut self, n: usize) -> Self {
        self.max_in_flight = Some(n);
        self
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
    // -- Connection state --
    disconnected: bool,
    peer_disconnected: bool,

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
    /// Cumulative credit count last actually sent to the peer. When the send
    /// queue is full, credit updates are deferred; because they carry an
    /// absolute count, the next send subsumes any skipped ones.
    credits_sent: usize,
    send_in_flight: usize,
    /// Send-queue depth (`max_send_wr`). Un-completed send WRs (data/padding
    /// Writes + credit Sends) must not exceed this, or `ibv_post_send` returns
    /// ENOMEM. See docs/bugs/ring-send-queue-exhaustion.md.
    sq_capacity: usize,

    // -- MW (dropped BEFORE QP — invalidation needs live QP) --
    _recv_mw: Option<MemoryWindow>,

    // -- QP (poster: owns only the QP; its CQs live in the sources below) --
    qp: AsyncQp,

    // -- Completion sources (own the send/recv CQs). Declared AFTER `qp` so
    //    RAII destroys the QP before its CQs (destroy-QP-before-CQ). --
    send_src: CompletionSource,
    recv_src: CompletionSource,

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

// ---------------------------------------------------------------------------
// PendingCreditRing — cancellation-safe setup transaction
// ---------------------------------------------------------------------------

/// The CM connection once the handshake has completed and it has been decomposed
/// into the pieces the steady-state transport holds. Field order = drop order:
/// `cm_id` before `event_channel` (`rdma_destroy_id` needs the channel fd open),
/// matching [`CreditRingTransport`].
struct PendingCm {
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,
}

/// The RDMA resource bundle a connection owns *during setup*, in verbs drop
/// order (MW → QP → CQs → MRs → CM). Holds only *resources* (no protocol
/// state), because the protocol state (remote ring addr/rkey/capacity) is not
/// known until the peer token arrives — [`PendingCreditRing::commit`] assembles
/// the full [`CreditRingTransport`] at that point.
struct PendingCreditResources {
    // MW (recv) — invalidated before the QP.
    recv_mw: Option<MemoryWindow>,
    // QP poster — destroyed before its CQs (held in the sources) and CM id.
    qp: AsyncQp,
    send_src: CompletionSource,
    recv_src: CompletionSource,
    // MRs.
    send_mr: OwnedMemoryRegion,
    recv_mr: OwnedMemoryRegion,
    /// Setup-only: the 20-byte peer-token landing buffer. Dropped at `commit`
    /// (the exchange is done); kept in the bundle during setup so a rollback
    /// after `to_error()` can drain its flush CQE against a live MR.
    token_recv_mr: OwnedMemoryRegion,
    doorbell_bufs: Box<[OwnedMemoryRegion]>,
    // CM (drop last) — populated once the handshake completes (`attach_cm`).
    pd: Arc<ProtectionDomain>,
    cm: Option<PendingCm>,
}

/// A credit-ring connection under construction (arm-park only). Owns every
/// resource created after the QP so that **any** `?`/cancellation between QP
/// creation and `commit()` rolls back through the same ownership protocol as
/// steady-state `Drop` — closing the window where a half-built connection would
/// drop its CQs before its QP, or destroy its QP while a still-borrowed CM id is
/// alive.
///
/// Declared **after** the caller's `async_cm`/`conn_id` local so that on an early
/// return before the CM is attached, this guard drops *first* — destroying the
/// QP while the CM id (still owned by the caller) is alive, satisfying the verbs
/// "destroy QP before its CM id" rule.
struct PendingCreditRing {
    /// The resource bundle; `take`n by `commit` (disarms rollback) or by `Drop`.
    res: Option<PendingCreditResources>,
    max_outstanding: usize,
    use_mr_rkey: bool,
    config: CreditRingConfig,
}

impl PendingCreditRing {
    #[inline]
    fn res_mut(&mut self) -> &mut PendingCreditResources {
        self.res
            .as_mut()
            .expect("PendingCreditRing used after commit")
    }

    /// Take ownership of the established CM connection's decomposed parts (fd +
    /// CM id + event channel). Infallible: the fallible `AsyncFd::new` is done by
    /// the caller on a borrow *before* this call (so the CM handle stays intact —
    /// and thus the QP's CM id stays alive — if it fails).
    fn attach_cm(&mut self, cm_async_fd: AsyncFd<RawFd>, cm_id: CmId, event_channel: EventChannel) {
        self.res_mut().cm = Some(PendingCm {
            cm_async_fd,
            cm_id,
            event_channel,
        });
    }

    /// Finish setup on the now-RTS QP: bind the recv Memory Window, exchange
    /// tokens, drain the setup send completions, and post the doorbell recvs.
    /// Returns the validated peer ring info for `commit`. Any error here rolls
    /// back through `Drop`.
    async fn finish(&mut self) -> crate::Result<(u64, u32, usize)> {
        let use_mr_rkey = self.use_mr_rkey;
        let ring_capacity = self.config.ring_capacity;
        // One monotonic deadline for the whole post-RTS token phase (token
        // exchange + setup-send drain), so a stalled peer/driver cannot hang
        // setup past `token_timeout`.
        let deadline = tokio::time::Instant::now() + self.config.token_timeout;
        let res = self.res_mut();

        // QP is RTS — bind Memory Window to scope remote write access (unless
        // using MR-rkey fallback).
        let (recv_mw, mw_rkey) = if use_mr_rkey {
            (None, res.recv_mr.rkey())
        } else {
            let (mw, rkey) = bind_recv_mw(&res.qp, &res.pd, &res.recv_mr, ring_capacity)?;
            (Some(mw), rkey)
        };
        res.recv_mw = recv_mw;

        // Complete token exchange (async — waits for CQ notification).
        let (remote_addr, remote_rkey, remote_capacity) = complete_token_exchange(
            &res.qp,
            &mut res.recv_src,
            &res.pd,
            &res.recv_mr,
            mw_rkey,
            &res.token_recv_mr,
            deadline,
            ring_capacity,
        )
        .await?;

        // Drain the exact setup send completions (token send + MW bind).
        let setup_wrs: &[u64] = if use_mr_rkey {
            &[WR_ID_TOKEN_SEND]
        } else {
            &[WR_ID_TOKEN_SEND, WR_ID_RECV_MW_BIND]
        };
        drain_ring_setup_sends(&mut res.send_src, setup_wrs, deadline).await?;

        // Post doorbell recv WRs (after token exchange is done).
        for (i, mr) in res.doorbell_bufs.iter().enumerate() {
            let sge = Sge::new(mr.addr(), 4, mr.lkey());
            let mut wr = RecvWr::new(i as u64).sg(sge);
            res.qp.post_recv_wr(&mut wr)?;
        }

        Ok((remote_addr, remote_rkey, remote_capacity))
    }

    /// Commit: assemble the full [`CreditRingTransport`] from the resource bundle
    /// plus the peer ring info, and hand ownership to a steady-state transport.
    /// Disarms the rollback `Drop`.
    fn commit(
        mut self,
        remote_addr: u64,
        remote_rkey: u32,
        remote_capacity: usize,
    ) -> CreditRingTransport {
        let res = self.res.take().expect("commit called twice");
        let PendingCreditResources {
            recv_mw,
            qp,
            send_src,
            recv_src,
            send_mr,
            recv_mr,
            token_recv_mr,
            doorbell_bufs,
            pd,
            cm,
        } = res;
        // Setup-only landing buffer: the exchange is complete.
        drop(token_recv_mr);
        let PendingCm {
            cm_async_fd,
            cm_id,
            event_channel,
        } = cm.expect("commit before attach_cm");

        CreditRingTransport::from_parts(
            qp,
            send_src,
            recv_src,
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
            self.max_outstanding,
            self.config.clone(),
        )
    }
}

impl Drop for PendingCreditRing {
    fn drop(&mut self) {
        // Committed → nothing to roll back.
        let Some(mut res) = self.res.take() else {
            return;
        };
        // Arm-park: private CQs, synchronous bounded drain (force `ERR` so the
        // flush terminates), then drop in field order (MW → QP → CQs → MRs → CM).
        let _ = res.qp.to_error();
        crate::transport_common::drain_flushed(&mut res.send_src, &mut res.recv_src);
        drop(res);
    }
}

/// Build the QP + resource bundle and open the setup transaction, shared by
/// `connect` and `accept`. On return the 20-byte peer-token recv WR is posted;
/// the caller performs its role-specific CM handshake, then
/// [`PendingCreditRing::attach_cm`] + [`finish`](PendingCreditRing::finish) +
/// [`commit`](PendingCreditRing::commit).
///
/// The caller must declare its CM endpoint (`async_cm` / `conn_id`) as a local
/// **before** the returned guard so that, on a cancellation before `attach_cm`,
/// the guard drops first — destroying the QP while the endpoint's CM id is still
/// alive (the QP was created against it here).
fn prepare_pending_credit(
    endpoint: &impl CmSetupEndpoint,
    config: &CreditRingConfig,
) -> crate::Result<PendingCreditRing> {
    let ctx = endpoint
        .verbs_context()
        .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
    let pd = endpoint.alloc_pd()?;

    // Resolve the effective remote-access mode against the routed device: bind a
    // Memory Window when supported, else fall back to the MR rkey (Auto), unless
    // the mode forces one or the other.
    let use_mr_rkey = resolve_mr_rkey(&pd, config.mw_mode)?;

    let max_outstanding = effective_max_outstanding(
        &pd,
        config.ring_capacity,
        config.max_message_size,
        config.max_in_flight,
        CREDIT_RING_SQ_HEADROOM,
    );
    let send_cq_depth = (max_outstanding + CREDIT_RING_SQ_HEADROOM) as i32;
    let recv_cq_depth = (max_outstanding + 2) as i32;

    let qp_attr = QpInitAttr {
        qp_type: QpType::Rc,
        max_send_wr: send_cq_depth as u32,
        max_recv_wr: recv_cq_depth as u32,
        max_send_sge: 1,
        max_recv_sge: 1,
        max_inline_data: config.max_inline_data.max(RING_TOKEN_SIZE as u32),
        sq_sig_all: true,
    };

    // Build the completion sources first so they *own* the CQs, then create the
    // QP against those CQs. Declaring the sources before the QP makes the QP drop
    // before its CQs on a setup failure/cancellation — the verbs "destroy QP
    // before its CQ" order.
    let send_src = CompletionSource::arm_park(AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?);
    let recv_src = CompletionSource::arm_park(AsyncCq::create_tokio(ctx, recv_cq_depth)?);
    let cmqp = endpoint.create_qp_with_cq(
        &pd,
        &qp_attr,
        Some(send_src.arm_park_cq()),
        Some(recv_src.arm_park_cq()),
    )?;

    // In MR-rkey fallback mode we never bind a Memory Window, so omit the
    // MW_BIND access flag (some NICs reject it when max_mw == 0).
    let mw_bind = if use_mr_rkey {
        AccessFlags::empty()
    } else {
        AccessFlags::MW_BIND
    };

    let send_mr = pd.reg_mr_owned(vec![0u8; config.ring_capacity], AccessFlags::LOCAL_WRITE)?;
    let recv_mr = pd.reg_mr_owned(
        vec![0u8; config.ring_capacity],
        AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE | mw_bind,
    )?;

    let doorbell_bufs: Box<[OwnedMemoryRegion]> = (0..max_outstanding)
        .map(|_| pd.reg_mr_owned(vec![0u8; 4], AccessFlags::LOCAL_WRITE))
        .collect::<crate::Result<Vec<_>>>()?
        .into_boxed_slice();

    // QP poster declared *after* the sources (see above).
    let qp = AsyncQp::new_poster(cmqp);

    // Post token recv FIRST (before doorbells, before the CM handshake). This
    // ensures the 20-byte token recv WR is consumed by the peer's token Send,
    // not by a 4-byte doorbell WR. Arm-park has no slot registration, so posting
    // before the guard takes ownership of the QP is fine.
    let token_recv_mr = post_token_recv(&qp, &pd)?;

    Ok(PendingCreditRing {
        res: Some(PendingCreditResources {
            recv_mw: None,
            qp,
            send_src,
            recv_src,
            send_mr,
            recv_mr,
            token_recv_mr,
            doorbell_bufs,
            pd,
            cm: None,
        }),
        max_outstanding,
        use_mr_rkey,
        config: config.clone(),
    })
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

        // Build the QP + resource bundle and open the transaction. `pending` is
        // declared *after* `async_cm` so an early return before `attach_cm` drops
        // the QP while `async_cm`'s CM id is still live (the QP was created
        // against it).
        let mut pending = prepare_pending_credit(&async_cm, &config)?;

        // Handshake: `connect` borrows `async_cm` (does not consume it), so a
        // cancellation here drops `pending` first (QP destroyed) then `async_cm`.
        async_cm.connect(&ConnParam::default()).await?;

        // Attach the established CM. The fallible `AsyncFd::new` runs on a borrow
        // *before* `into_parts`, so `async_cm` stays intact (CM id alive) if it
        // fails.
        let cm_async_fd =
            AsyncFd::new(async_cm.event_channel().fd()).map_err(crate::Error::Verbs)?;
        let (event_channel, cm_id) = async_cm.into_parts();
        pending.attach_cm(cm_async_fd, cm_id, event_channel);

        // Bind MW, exchange tokens, drain setup sends, post doorbells.
        let (remote_addr, remote_rkey, remote_capacity) = pending.finish().await?;

        Ok(pending.commit(remote_addr, remote_rkey, remote_capacity))
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

        // Build the QP + resource bundle and open the transaction. `pending` is
        // declared *after* `conn_id` so an early return before `attach_cm` drops
        // the QP while `conn_id`'s CM id is still live (the QP was created
        // against it).
        let mut pending = prepare_pending_credit(&conn_id, &config)?;

        // Phased accept handshake so `conn_id` is never moved into a consuming
        // future: send the reply (sync), await ESTABLISHED (this borrows only the
        // `listener`, NOT `conn_id`), then migrate `conn_id` (sync). Because
        // `conn_id` stays a local declared before `pending`, a cancellation
        // during the await drops `pending` first — destroying the QP before
        // `conn_id`'s CM id — closing the accept teardown-order gap.
        conn_id.accept(&ConnParam::default())?;
        listener.await_established().await?;

        let conn_ch = EventChannel::new()?;
        conn_ch.set_nonblocking()?;
        conn_id.migrate(&conn_ch)?;
        // Fallible `AsyncFd::new` on a borrow before consuming `conn_id`/`conn_ch`
        // into the bundle, so both stay live (QP's CM id alive) if it fails.
        let cm_async_fd = AsyncFd::new(conn_ch.fd()).map_err(crate::Error::Verbs)?;
        pending.attach_cm(cm_async_fd, conn_id, conn_ch);

        // Bind MW, exchange tokens, drain setup sends, post doorbells.
        let (remote_addr, remote_rkey, remote_capacity) = pending.finish().await?;

        Ok(pending.commit(remote_addr, remote_rkey, remote_capacity))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        qp: AsyncQp,
        send_src: CompletionSource,
        recv_src: CompletionSource,
        cm_async_fd: AsyncFd<RawFd>,
        cm_id: CmId,
        event_channel: EventChannel,
        pd: Arc<ProtectionDomain>,
        send_mr: OwnedMemoryRegion,
        recv_mr: OwnedMemoryRegion,
        recv_mw: Option<MemoryWindow>,
        doorbell_bufs: Box<[OwnedMemoryRegion]>,
        remote_addr: u64,
        remote_rkey: u32,
        remote_capacity: usize,
        max_outstanding: usize,
        config: CreditRingConfig,
    ) -> Self {
        let ring_capacity = config.ring_capacity;
        Self {
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
            credits_sent: 0,
            send_in_flight: 0,
            sq_capacity: max_outstanding + CREDIT_RING_SQ_HEADROOM,

            _recv_mw: recv_mw,

            qp,

            send_src,
            recv_src,

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

    /// Repost a doorbell recv buffer (round-robin).
    fn repost_doorbell(&mut self) -> crate::Result<()> {
        crate::transport_common::repost_doorbell(
            &self.qp,
            &self.doorbell_bufs,
            &mut self.doorbell_repost_idx,
        )
    }

    /// Non-blocking drain of recv CQ for credit updates only.
    /// Called from send_copy when credits are exhausted, to pick up any
    /// credit Send+Imm completions without requiring the caller to poll_recv.
    /// Data completions are stashed for the next poll_recv call.
    fn drain_recv_credits(&mut self) {
        let mut wc_buf = [WorkCompletion::default(); 8];
        // Arm + drain (one pass, non-blocking).
        let n = match self.recv_src.drain_once(&mut wc_buf) {
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
                    let (offset, length) = crate::transport_common::decode_ring_imm(imm);
                    if length == 0 {
                        // Padding — advance recv_ring head, repost doorbell.
                        // Padding consumes a sender credit, so assign a tracker
                        // slot and immediately release it.
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
                            if self.flush_credits().is_err() {
                                self.peer_disconnected = true;
                                return;
                            }
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

    /// Post a cumulative credit update to the peer if credits are pending and
    /// the send queue has room. Credit `Send`s carry the absolute freed count,
    /// so a single update subsumes any deferred ones; when the queue is full we
    /// skip and retry after a send completes (see `poll_send_completion`). This
    /// keeps `send_in_flight` within the send-queue depth so `ibv_post_send`
    /// never returns ENOMEM. See docs/bugs/ring-send-queue-exhaustion.md.
    ///
    /// The tight `send_copy` `+ 2` backpressure accounting relies on this only
    /// ever being called from the completion/release paths
    /// (`poll_send_completion` and the recv-side slot release), never mid
    /// `send_copy`. If that assumption changes, a concurrently posted credit
    /// could consume a slot `send_copy` had reserved for its padding+data pair
    /// and overflow the send queue.
    fn flush_credits(&mut self) -> crate::Result<()> {
        if self.local_freed_credits > self.credits_sent && self.send_in_flight < self.sq_capacity {
            let imm = self.local_freed_credits as u32;
            let wr_id = WR_ID_CREDIT_FLAG | (self.local_freed_credits as u64);
            let mut wr = SendWr::new(wr_id, WrOpcode::SendWithImm(imm)).flags(SendFlags::SIGNALED);
            self.qp.post_send_wr(&mut wr)?;
            self.credits_sent = self.local_freed_credits;
            self.send_in_flight += 1;
            debug_assert!(
                self.send_in_flight <= self.sq_capacity,
                "send queue overflow after credit post: {} > {}",
                self.send_in_flight,
                self.sq_capacity
            );
        }
        Ok(())
    }

    /// Process a batch of send-CQ work completions: release `send_ring` for data
    /// WRs and decrement `send_in_flight`. Credit/padding WRs occupy no local
    /// ring space. Returns `Err` (and marks the peer disconnected) on a failed
    /// completion. Shared by [`poll_send_completion`](Transport::poll_send_completion)
    /// and [`drain_send_cq`](Self::drain_send_cq).
    fn process_send_completions(&mut self, wcs: &[WorkCompletion]) -> crate::Result<()> {
        for wc in wcs {
            if !wc.is_success() {
                self.peer_disconnected = true;
                return Err(crate::Error::WorkCompletion {
                    status: wc.status_raw(),
                    vendor_err: wc.vendor_err(),
                });
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
        Ok(())
    }

    /// Non-blocking reap of the local send CQ (direct `ibv_poll_cq`, no
    /// notification arm). A pure-reader stream drives only `poll_recv` /
    /// `repost_recv` and never calls `poll_send_completion`, so its credit
    /// `Send` completions would otherwise accumulate until the send queue
    /// saturates (`send_in_flight == sq_capacity`) and it can no longer post
    /// credit updates — stalling the writer, which never regains credits.
    /// `poll_recv` calls this so a reader keeps its own send queue drained.
    /// Safe alongside the async send path: the stream is driven by a single
    /// task, and this uses a plain poll (no `req_notify`), so it does not
    /// disturb `send_cq_state`.
    fn drain_send_cq(&mut self) -> crate::Result<()> {
        let mut wc_buf = [WorkCompletion::default(); 8];
        loop {
            let n = self.send_src.try_drain(&mut wc_buf)?;
            if n == 0 {
                break;
            }
            self.process_send_completions(&wc_buf[..n])?;
        }
        Ok(())
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
        self.send_gather(&[std::io::IoSlice::new(data)])
    }

    fn send_gather(&mut self, bufs: &[std::io::IoSlice<'_>]) -> crate::Result<usize> {
        if self.peer_disconnected {
            return Err(crate::Error::WorkCompletion {
                status: rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR,
                vendor_err: 0,
            });
        }
        let total_len: usize = bufs.iter().map(|b| b.len()).sum();
        if total_len == 0 {
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

        // Send-queue backpressure. A single send_copy posts up to 2 WRs
        // (padding + data), and credit Sends also consume the queue. If the
        // send queue can't hold them, ibv_post_send would return ENOMEM.
        // Return Ok(0) (transient "cannot accept now", not a fatal error) so the
        // caller reaps via poll_send_completion and retries — matching the
        // Transport::send_copy contract. (Reaping is left to
        // poll_send_completion's CqPollState path; polling the CQ here would
        // desync the edge-triggered notification and lose wakeups.)
        // See docs/bugs/ring-send-queue-exhaustion.md.
        if self.send_in_flight + 2 > self.sq_capacity {
            return Ok(0);
        }

        // Clamp data length (16-bit immediate encoding limit).
        let data_len = total_len
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
            let imm = crate::transport_common::encode_ring_imm(pad_remote_offset, 0);
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

        // Gather the caller's slices into send_ring at the reserved offset
        // (one copy straight into the registered ring, no scratch).
        crate::transport_common::gather_into(
            &mut self.send_ring.mr.as_mut_slice()[local_offset..local_offset + data_len],
            bufs,
        );

        // Build RDMA Write+Imm WR for data.
        let remote_offset = self.remote_write_tail;
        let imm = crate::transport_common::encode_ring_imm(remote_offset, data_len);
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

        debug_assert!(
            self.send_in_flight <= self.sq_capacity,
            "send queue overflow after data post: {} > {}",
            self.send_in_flight,
            self.sq_capacity
        );

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
        let n = match self.send_src.poll_completions(cx, &mut wc_buf) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => n,
        };
        if let Err(e) = self.process_send_completions(&wc_buf[..n]) {
            return Poll::Ready(Err(e));
        }
        // Reaping freed send-queue slots; flush any credit update that was
        // deferred while the queue was full.
        if let Err(e) = self.flush_credits() {
            self.peer_disconnected = true;
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(()))
    }

    fn sends_in_flight(&self) -> usize {
        self.send_in_flight
    }

    fn recv_window(&self) -> usize {
        self.max_outstanding
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

        // Reap our own send CQ (non-blocking) so a pure-reader stream drains its
        // credit `Send` completions and can keep posting credit updates. Without
        // this, `send_in_flight` saturates and the writer stalls waiting for
        // credits it never regains. Also flush any credit deferred while full.
        if let Err(e) = self.drain_send_cq() {
            self.peer_disconnected = true;
            return Poll::Ready(Err(e));
        }
        if let Err(e) = self.flush_credits() {
            self.peer_disconnected = true;
            return Poll::Ready(Err(e));
        }

        // Poll recv CQ. Loop to handle batches that contain only credit/padding
        // completions — we must re-poll the CQ rather than returning Pending
        // without a waker registration.
        loop {
            let mut wc_buf = [WorkCompletion::default(); 8];
            let n = match self.recv_src.poll_completions(cx, &mut wc_buf) {
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
                        let (offset, length) = crate::transport_common::decode_ring_imm(imm);

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
                                if self.flush_credits().is_err() {
                                    self.peer_disconnected = true;
                                    return Poll::Ready(Err(crate::Error::InvalidArg(
                                        "credit post_send failed".into(),
                                    )));
                                }
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

            // Send credit update (absolute freed count). Deferred if the send
            // queue is full; flushed later from poll_send_completion.
            self.flush_credits()?;

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
        crate::transport_common::poll_cm_disconnect(
            &self.cm_async_fd,
            &self.event_channel,
            &mut self.peer_disconnected,
            cx,
        )
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
        // Force the QP to ERROR so every outstanding WR flushes locally, then
        // reap the CQEs through the sources so the QP releases its MR references
        // before the MRs are deregistered (field drop order: MW → QP → sources
        // → MRs). Forcing ERROR first avoids the race where the async
        // `cm_id.disconnect()` has not yet flushed the WRs. Bounded so `Drop`
        // never blocks the calling (possibly tokio worker) thread; the flush is
        // local (microseconds) and `ibv_destroy_qp` is the backstop.
        let _ = self.qp.to_error();
        crate::transport_common::drain_flushed(&mut self.send_src, &mut self.recv_src);
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

#[cfg(test)]
mod tests {
    use super::CreditRingConfig;

    // Default derives the slot count from message size (max_in_flight unset).
    #[test]
    fn default_has_no_explicit_in_flight() {
        assert_eq!(CreditRingConfig::default().max_in_flight, None);
    }

    // with_max_in_flight sets an explicit pipeline budget.
    #[test]
    fn with_max_in_flight_sets_budget() {
        let c = CreditRingConfig::default().with_max_in_flight(256);
        assert_eq!(c.max_in_flight, Some(256));
    }
}
