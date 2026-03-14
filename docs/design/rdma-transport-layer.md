# RDMA Transport Layer — Shared Abstraction Design

**Status:** Design Proposal (reviewed)  
**Date:** 2026-03-14  
**Last Review:** 2026-03-14  
**Goal:** Extract a shared transport layer that both `AsyncRdmaStream` (byte stream for gRPC)
and `RdmaUdpSocket` (datagram socket for Quinn) can build on, eliminating duplication and
enabling future transport variants.

## Problem

Today, `AsyncRdmaStream` directly manages QP creation, buffer allocation, CQ polling, CM events,
and disconnect detection — ~600 lines tightly coupling stream semantics to RDMA mechanics.
The proposed `RdmaUdpSocket` needs the same RDMA mechanics but with datagram semantics. Without
a shared layer, we'd duplicate QP setup, CQ polling, buffer management, disconnect detection,
and CM event handling.

## Current Architecture (No Shared Layer)

```
AsyncRdmaStream (byte stream)         RdmaUdpSocket (datagram)
  ├── QP setup                          ├── QP setup          ← duplicated
  ├── Buffer alloc (8 recv, 1 send)     ├── Buffer alloc      ← duplicated
  ├── CQ polling (send + recv)          ├── CQ polling        ← duplicated
  ├── CM event / disconnect             ├── CM event          ← duplicated
  ├── Partial read buffering            ├── Multi-QP map
  └── Graceful close                    └── Batch recv
        │                                     │
  AsyncQp / AsyncCq / AsyncCmId         AsyncQp / AsyncCq / AsyncCmId
```

## Proposed Architecture (Shared Transport Layer)

```
AsyncRdmaStream (byte stream)         RdmaUdpSocket (datagram)
  ├── Partial read buffering            ├── Multi-QP connection map
  ├── AsyncWrite / AsyncRead            ├── AsyncUdpSocket / UdpSender
  └── Graceful close                    └── Batch recv
        │                                     │
        └──────────┬──────────────────────────┘
                   │
          RdmaTransport (shared)
            ├── Buffer pool (configurable count + size)
            ├── Send / recv operations
            ├── CQ poll wrappers
            ├── Disconnect detection
            └── Connection lifecycle
                   │
          AsyncQp / AsyncCq / AsyncCmId (primitives)
```

## Design: Transport Trait + RdmaTransport

### Core Trait

The `Transport` trait is the foundational abstraction. **All consumers are generic over
`T: Transport` from day one** — not a future refactor. This enables:

- Mock transports for unit testing (no RDMA hardware needed)
- Future Ring buffer transport as a drop-in replacement
- Clean separation: transport handles RDMA mechanics, consumers handle protocol semantics

The trait is designed around the **consumer's view** (send bytes, receive bytes) rather than
RDMA verbs. It uses transport-neutral completion types — `WorkCompletion` stays internal.

```rust
/// A completed receive operation — transport-neutral.
#[derive(Default, Clone)]
pub struct RecvCompletion {
    pub buf_idx: usize,
    pub byte_len: usize,
}

pub trait Transport: Send + Sync {
    // --- Send path ---

    /// Copy data into an internal send buffer and post it.
    ///
    /// Returns bytes accepted (capped by transport capacity).
    /// May return `Ok(0)` if transport is full — retry after
    /// `poll_send_completion` returns.
    fn send_copy(&mut self, data: &[u8]) -> Result<usize>;

    /// Wait for at least one in-flight send to complete.
    /// Returns Err on fatal CQ error (FLUSH_ERR → marks qp_dead).
    fn poll_send_completion(&mut self, cx: &mut Context) -> Poll<Result<()>>;

    // --- Recv path ---

    /// Poll for recv completions. Returns count + fills `out` with
    /// buf_idx and byte_len for each completed receive.
    ///
    /// Implementations must internally stash excess completions
    /// that don't fit in `out` (Ring transport loses data otherwise).
    /// Returns Err on fatal CQ error (FLUSH_ERR → marks qp_dead).
    fn poll_recv(
        &mut self, cx: &mut Context, out: &mut [RecvCompletion],
    ) -> Poll<Result<usize>>;

    /// Access received data by buffer index (valid until repost_recv).
    fn recv_buf(&self, buf_idx: usize) -> &[u8];

    /// Release a recv buffer for reuse.
    fn repost_recv(&mut self, buf_idx: usize) -> Result<()>;

    // --- Connection lifecycle ---

    /// Register task waker for disconnect events AND check state.
    /// Returns true if connection is dead (caller should EOF/BrokenPipe).
    ///
    /// Must be called when CQ returns Pending — ensures disconnect
    /// events wake the task even when CQ fd has nothing.
    fn poll_disconnect(&mut self, cx: &mut Context) -> bool;

    /// True when connection is dead — no more data will ever arrive.
    fn is_qp_dead(&self) -> bool;

    /// Initiate graceful disconnect. Idempotent.
    fn disconnect(&mut self) -> Result<()>;

    // --- Metadata ---

    fn local_addr(&self) -> Option<SocketAddr>;
    fn peer_addr(&self) -> Option<SocketAddr>;
}
```

**Design rationale — why this shape:**

- **10 methods, 1 type.** Every method is called by at least one consumer. No dead weight.

- **No `SendCompletion` type.** No consumer reads send completion details — they just need
  to know "a send finished." `poll_send_completion` returns `Poll<Result<()>>` (not a batch).
  Internally, the transport may drain multiple CQ entries per call; it just doesn't expose them.

- **`poll_recv` is batched, `poll_send_completion` is not.** Recv batching matters for datagram
  (fill 8+ packets per poll_recv call across connections). Send is always wait-for-one
  (stream: sequential writes; datagram: one send per poll_send call).

- **`poll_disconnect` replaces `register_cm_waker_and_check`.** Clearer name. Registers the
  waker on the disconnect event channel, then checks if the connection is already dead. The
  combined operation avoids a TOCTOU race (register → check, not check → register).

- **No `mark_qp_dead`.** The transport marks itself dead internally when it encounters
  FLUSH_ERR in `poll_recv`/`poll_send_completion`. Consumers check `is_qp_dead()` or handle
  `Err()` returns — they never need to set the flag manually.

- **No `max_send_size` / `max_recv_capacity`.** No consumer calls them. Available as methods
  on concrete types (`RdmaTransport::buf_size()`) for configuration, but not on the trait.

- **`send_copy` may return `Ok(0)`.** Ring transport: remote ring full. Send/Recv: never
  happens (hardware RNR). Consumer retry loop handles both.

### Factory: Construction is separate from the trait

The trait covers data-path operations only. Construction is type-specific:

```rust
// Consumers take a pre-constructed transport — generic over T
let transport = RdmaTransport::connect(addr, TransportConfig::stream()).await?;
let stream = AsyncRdmaStream::new(transport);

// Or with a future Ring transport — same consumer code
let transport = RdmaRingTransport::connect(addr, RingConfig::default()).await?;
let stream = AsyncRdmaStream::new(transport);
```

### TransportConfig (for RdmaTransport)

```rust
/// Configuration for creating an RDMA transport.
pub struct TransportConfig {
    /// Size of each buffer (send and recv).
    pub buf_size: usize,
    /// Number of pre-posted recv buffers.
    pub num_recv_bufs: usize,
    /// Number of send buffers (1 for stream, N for datagram).
    pub num_send_bufs: usize,
    /// Max inline data size (0 = disabled).
    pub max_inline_data: u32,
    /// QP type (RC for both initially, UD possible future).
    pub qp_type: QpType,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            buf_size: 64 * 1024,     // 64 KB
            num_recv_bufs: 8,        // Stream default
            num_send_bufs: 1,        // Stream default
            max_inline_data: 0,
            qp_type: QpType::Rc,
        }
    }
}

impl TransportConfig {
    /// Configuration tuned for datagram workloads (Quinn/QUIC).
    pub fn datagram() -> Self {
        Self {
            buf_size: 1500,          // ~MTU sized
            num_recv_bufs: 64,       // Absorb bursts between polls
            num_send_bufs: 4,        // Multiple senders need buffers
            max_inline_data: 64,     // Small QUIC packets benefit
            qp_type: QpType::Rc,
        }
    }

    /// Configuration tuned for byte stream workloads (gRPC/tonic).
    pub fn stream() -> Self {
        Self {
            buf_size: 64 * 1024,     // 64 KB for throughput
            num_recv_bufs: 8,        // Matches current design
            num_send_bufs: 1,        // Single writer
            max_inline_data: 0,
            qp_type: QpType::Rc,
        }
    }
}
```

### RdmaTransport — `impl Transport` (Send/Recv)

```rust
/// A single RDMA connection's transport resources.
///
/// Owns the QP, buffers, and CQ state. Does NOT own protocol semantics
/// (byte stream vs datagram) — those are layered on top.
///
/// **Drop order is critical.** Fields drop in declaration order:
/// 1. State fields (no RDMA teardown)
/// 2. MRs (deregister before QP destroy)
/// 3. PD (ref-counted, safe anytime)
/// 4. QP (destroys QP, needs CQs alive — CQs are inside AsyncQp)
/// 5. cm_async_fd (deregister from epoll BEFORE closing the fd)
/// 6. cm_id (disconnect/destroy)
/// 7. event_channel (closes the fd — must be LAST)
pub struct RdmaTransport {
    // CQ poll state (no RDMA teardown)
    send_cq_state: CqPollState,
    recv_cq_state: CqPollState,

    // Connection state (no RDMA teardown)
    disconnected: bool,
    /// QP is dead — no more data will ever arrive. Set on WR_FLUSH_ERR
    /// or when QP state transitions away from RTS.
    qp_dead: bool,
    /// Peer sent DREQ — disconnect event received. Data may still be
    /// in the CQ (draining). Distinct from `qp_dead`.
    peer_disconnected: bool,

    // RDMA data-path resources (drop order: MRs → PD → QP)
    send_bufs: Vec<OwnedMemoryRegion>,
    recv_bufs: Vec<OwnedMemoryRegion>,
    _pd: Arc<ProtectionDomain>,
    qp: AsyncQp,

    // CM resources — drop order: AsyncFd → CmId → EventChannel
    // cm_async_fd MUST drop before event_channel (which owns the fd).
    // Dropping event_channel first would close the fd, then cm_async_fd
    // would try to deregister a stale fd from epoll → use-after-close.
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,

    // Config
    config: TransportConfig,
}
```

### Factory Methods

```rust
impl RdmaTransport {
    /// Connect to a remote peer (client side).
    pub async fn connect(
        addr: &SocketAddr,
        config: TransportConfig,
    ) -> Result<Self> {
        // 1. CM connection
        let async_cm = AsyncCmId::new(PortSpace::Tcp)?;
        async_cm.resolve_addr(None, addr, 2000).await?;
        async_cm.resolve_route(2000).await?;

        // 2. Allocate PD
        let ctx = async_cm.verbs_context().ok_or(Error::NoDevice)?;
        let pd = async_cm.alloc_pd()?;

        // 3. Create dual CQs
        let send_cq_depth = config.num_send_bufs as i32 + 1;
        let recv_cq_depth = config.num_recv_bufs as i32 + 1;
        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        // 4. Build QP init attributes from config
        let qp_attr = QpInitAttr {
            qp_type: config.qp_type,
            max_send_wr: config.num_send_bufs as u32 + 1,
            max_recv_wr: config.num_recv_bufs as u32 + 1,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: config.max_inline_data,
            sq_sig_all: true,
        };

        // 5. Create QP
        let cmqp = async_cm.create_qp_with_cq(
            &pd, &qp_attr,
            Some(send_cq.cq()), Some(recv_cq.cq()),
        )?;
        let qp = AsyncQp::new(cmqp, send_cq, recv_cq);

        // 6. Allocate buffers
        let access = AccessFlags::LOCAL_WRITE;
        let send_bufs = (0..config.num_send_bufs)
            .map(|_| pd.reg_mr_owned(vec![0u8; config.buf_size], access))
            .collect::<Result<Vec<_>>>()?;
        let recv_bufs = (0..config.num_recv_bufs)
            .map(|_| pd.reg_mr_owned(vec![0u8; config.buf_size], access))
            .collect::<Result<Vec<_>>>()?;

        // 7. Pre-post all recv buffers
        for (i, mr) in recv_bufs.iter().enumerate() {
            qp.post_recv_buffer(mr, i as u64)?;
        }

        // 8. CM connect
        async_cm.connect(&ConnParam::default()).await?;

        // 9. Extract CM parts
        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd())?;

        Ok(Self {
            send_cq_state: CqPollState::default(),
            recv_cq_state: CqPollState::default(),
            disconnected: false,
            qp_dead: false,
            peer_disconnected: false,
            send_bufs,
            recv_bufs,
            _pd: pd,
            qp,
            cm_async_fd,
            cm_id,
            event_channel,
            config,
        })
    }

    /// Accept a connection from a listener (server side).
    ///
    /// Follows the two-phase accept protocol from AsyncCmListener:
    /// 1. get_request() → new CM ID for this connection
    /// 2. Create QP + allocate buffers on the new CM ID
    /// 3. complete_accept() → handshake + rdma_migrate_id (decouples
    ///    from listener's event channel — critical! Without migration,
    ///    dropping the listener would kill accepted connections.)
    pub async fn accept(
        listener: &AsyncCmListener,
        config: TransportConfig,
    ) -> Result<Self> {
        // Phase 1: Await CONNECT_REQUEST
        let conn_id = listener.get_request().await?;

        // Phase 2: Set up QP on the new CM ID
        let ctx = conn_id.verbs_context().ok_or(Error::NoDevice)?;
        let pd = conn_id.alloc_pd()?;

        let send_cq_depth = config.num_send_bufs as i32 + 1;
        let recv_cq_depth = config.num_recv_bufs as i32 + 1;
        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        let qp_attr = QpInitAttr {
            qp_type: config.qp_type,
            max_send_wr: config.num_send_bufs as u32 + 1,
            max_recv_wr: config.num_recv_bufs as u32 + 1,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: config.max_inline_data,
            sq_sig_all: true,
        };

        let cmqp = conn_id.create_qp_with_cq(
            &pd, &qp_attr,
            Some(send_cq.cq()), Some(recv_cq.cq()),
        )?;
        let qp = AsyncQp::new(cmqp, send_cq, recv_cq);

        let access = AccessFlags::LOCAL_WRITE;
        let send_bufs = (0..config.num_send_bufs)
            .map(|_| pd.reg_mr_owned(vec![0u8; config.buf_size], access))
            .collect::<Result<Vec<_>>>()?;
        let recv_bufs = (0..config.num_recv_bufs)
            .map(|_| pd.reg_mr_owned(vec![0u8; config.buf_size], access))
            .collect::<Result<Vec<_>>>()?;

        // Pre-post all recv buffers BEFORE accept handshake
        for (i, mr) in recv_bufs.iter().enumerate() {
            qp.post_recv_buffer(mr, i as u64)?;
        }

        // Phase 3: Accept + migrate (decouples from listener's event channel)
        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd())?;

        Ok(Self {
            send_cq_state: CqPollState::default(),
            recv_cq_state: CqPollState::default(),
            disconnected: false,
            qp_dead: false,
            peer_disconnected: false,
            send_bufs,
            recv_bufs,
            _pd: pd,
            qp,
            cm_async_fd,
            cm_id,
            event_channel,
            config,
        })
    }
}
```

### Send / Recv Operations

These implement the `Transport` trait for the Send/Recv concrete type:

```rust
impl RdmaTransport {
    // --- Internal send buffer management ---

    /// Next send buffer index (round-robin). Plain usize — &mut self = exclusive access.
    next_send_idx: usize,  // Add to struct fields
    /// Tracks which send buffers are in-flight (awaiting CQ completion).
    send_in_flight: Vec<bool>,  // Add to struct fields, init to vec![false; num_send_bufs]

    /// impl Transport::send_copy
    /// Returns Ok(0) when all send buffers are occupied (caller should
    /// call poll_send_completion then retry).
    pub fn send_copy(&mut self, data: &[u8]) -> Result<usize> {
        // Find a free send buffer (round-robin with in-flight check)
        let n = self.send_bufs.len();
        let mut idx = self.next_send_idx % n;
        for _ in 0..n {
            if !self.send_in_flight[idx] {
                break;
            }
            idx = (idx + 1) % n;
        }
        if self.send_in_flight[idx] {
            return Ok(0);  // All send buffers occupied
        }

        let mr = &mut self.send_bufs[idx];
        let len = data.len().min(mr.as_slice().len());
        mr.as_mut_slice()[..len].copy_from_slice(&data[..len]);
        let wr_id = self.config.num_recv_bufs as u64 + idx as u64;
        self.qp.post_send_signaled(mr, 0, len, wr_id)?;
        self.send_in_flight[idx] = true;
        self.next_send_idx = (idx + 1) % n;
        Ok(len)
    }

    /// impl Transport::poll_send_completion
    /// Waits for at least one send CQ completion. Drains all ready completions
    /// and marks their buffers as free for reuse.
    pub fn poll_send_completion(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<()>> {
        let mut wc_buf = [WorkCompletion::default(); 4];
        let n = ready!(self.qp.poll_send_cq(cx, &mut self.send_cq_state, &mut wc_buf))?;
        for i in 0..n {
            if !wc_buf[i].is_success() {
                self.qp_dead = true;
                return Poll::Ready(Err(crate::Error::Verbs(io::Error::other(
                    format!("send WC error: status={:?}", wc_buf[i].status())
                ))));
            }
            // Mark this send buffer as free
            let wr_id = wc_buf[i].wr_id() as usize;
            let buf_idx = wr_id - self.config.num_recv_bufs;
            if buf_idx < self.send_in_flight.len() {
                self.send_in_flight[buf_idx] = false;
            }
        }
        Poll::Ready(Ok(()))
    }

    /// impl Transport::poll_recv
    /// Converts internal WorkCompletions to transport-neutral RecvCompletions.
    pub fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut [RecvCompletion],
    ) -> Poll<Result<usize>> {
        let mut wc_buf = vec![WorkCompletion::default(); out.len()];
        let n = ready!(self.qp.poll_recv_cq(cx, &mut self.recv_cq_state, &mut wc_buf))?;
        for i in 0..n {
            if !wc_buf[i].is_success() {
                self.qp_dead = true;
                return Poll::Ready(Err(crate::Error::Verbs(io::Error::other(
                    format!("recv WC error: status={:?} wr_id={}",
                        wc_buf[i].status(), wc_buf[i].wr_id())
                ))));
            }
            out[i] = RecvCompletion {
                buf_idx: wc_buf[i].wr_id() as usize,
                byte_len: wc_buf[i].byte_len() as usize,
            };
        }
        Poll::Ready(Ok(n))
    }

    /// impl Transport::recv_buf
    pub fn recv_buf(&self, buf_idx: usize) -> &[u8] {
        self.recv_bufs[buf_idx].as_slice()
    }

    /// impl Transport::repost_recv
    pub fn repost_recv(&mut self, buf_idx: usize) -> Result<()> {
        self.qp.post_recv_buffer(&self.recv_bufs[buf_idx], buf_idx as u64)
    }

    // --- Concrete-type-only methods (not on Transport trait) ---

    /// Maximum bytes a single send_copy can accept.
    pub fn buf_size(&self) -> usize {
        self.config.buf_size
    }

    /// Number of pre-posted recv buffers.
    pub fn num_recv_bufs(&self) -> usize {
        self.config.num_recv_bufs
    }
}
```

### Disconnect Detection (Shared)

Both stream and datagram need to detect peer disconnect. There are **two distinct states**:

- **`peer_disconnected`**: DISCONNECTED CM event received. Data may still be draining from CQ.
- **`qp_dead`**: QP entered ERROR state (WR_FLUSH_ERR seen, or QP state ≠ RTS). No more data.

This distinction matters: after DISCONNECTED, the CQ may still have completions from
in-flight messages. Only after `qp_dead` should consumers return EOF.

```rust
impl RdmaTransport {
    /// True when QP is dead — no more data will ever arrive.
    pub fn is_qp_dead(&self) -> bool {
        self.qp_dead
    }

    /// True when peer sent DREQ. Data may still be in CQ.
    pub fn is_peer_disconnected(&self) -> bool {
        self.peer_disconnected
    }

    /// impl Transport::poll_disconnect
    ///
    /// Register the task waker on CM event channel fd AND check for
    /// disconnect. Call when CQ returns Pending — ensures disconnect
    /// events wake the task when CQ fd has nothing.
    ///
    /// Returns true if connection is dead (caller should EOF/BrokenPipe).
    pub fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> bool {
        if self.qp_dead {
            return true;
        }

        // Loop to drain spurious readiness (edge-triggered epoll).
        loop {
            match self.cm_async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(mut guard)) => {
                    guard.clear_ready();
                    if self.check_cm_event() {
                        return true;
                    }
                    // Spurious readiness or non-disconnect event consumed;
                    // loop to re-register the waker.
                }
                Poll::Pending => return false, // waker registered — will be woken on CM event
                Poll::Ready(Err(_)) => {
                    self.qp_dead = true;
                    return true;
                }
            }
        }
    }

    fn check_cm_event(&mut self) -> bool {
        match self.event_channel.try_get_event() {
            Ok(ev) => {
                let etype = ev.event_type();
                ev.ack();
                if etype == CmEventType::Disconnected {
                    self.peer_disconnected = true;
                }
                // Any CM event (Disconnected, Rejected, etc.) — check QP state
                if is_qp_dead(self.qp.as_raw()) {
                    self.qp_dead = true;
                }
                self.peer_disconnected || self.qp_dead
            }
            Err(crate::Error::WouldBlock) => {
                // No CM event — fall back to QP state check.
                if is_qp_dead(self.qp.as_raw()) {
                    self.qp_dead = true;
                    return true;
                }
                false
            }
            Err(_) => {
                // Event channel error — connection is dead.
                self.qp_dead = true;
                true
            }
        }
    }

    /// impl Transport::disconnect — initiate graceful disconnect.
    pub fn disconnect(&mut self) -> Result<()> {
        if !self.disconnected {
            self.cm_id.disconnect()?;
            self.disconnected = true;
        }
        Ok(())
    }

    /// Local socket address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.cm_id.local_addr()
    }

    /// Remote peer address.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.cm_id.peer_addr()
    }
}

impl Drop for RdmaTransport {
    fn drop(&mut self) {
        // Best-effort disconnect so the peer sees DREQ promptly.
        // Skip if already disconnected via explicit close.
        if !self.disconnected {
            let _ = self.cm_id.disconnect();
        }
    }
}
```

## How Consumers Use Transport

Consumers are **generic over `T: Transport`** from day one. This means:
- `AsyncRdmaStream<RdmaTransport>` for production Send/Recv
- `AsyncRdmaStream<MockTransport>` for unit tests (no RDMA hardware)
- `AsyncRdmaStream<RdmaRingTransport>` in the future

### AsyncRdmaStream (Byte Stream)

```rust
pub struct AsyncRdmaStream<T: Transport> {
    transport: T,
    // Stream-specific state only:
    recv_pending: Option<(usize, usize, usize)>,  // Partial read buffer
    write_pending: Option<usize>,                  // In-flight send length
}

impl<T: Transport> AsyncRdmaStream<T> {
    /// Wrap a pre-constructed transport as a byte stream.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            recv_pending: None,
            write_pending: None,
        }
    }
}

/// Convenience: connect with the default Send/Recv transport.
impl AsyncRdmaStream<RdmaTransport> {
    pub async fn connect(addr: &SocketAddr) -> Result<Self> {
        let transport = RdmaTransport::connect(
            addr,
            TransportConfig::stream(),    // 8 recv × 64KB, 1 send × 64KB
        ).await?;
        Ok(Self::new(transport))
    }
}

// Unpin is required for self.get_mut() in poll_read/poll_write.
// RdmaTransport is Unpin (all fields are Unpin). Generic T: Transport
// may not be, so we assert it.
impl<T: Transport> Unpin for AsyncRdmaStream<T> {}

impl<T: Transport> AsyncRead for AsyncRdmaStream<T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8])
        -> Poll<io::Result<usize>>
    {
        let this = self.get_mut();

        // Short-circuit: empty buffer or QP dead → return immediately.
        if buf.is_empty() || this.transport.is_qp_dead() {
            return Poll::Ready(Ok(0));
        }

        // Phase 1: Return partial buffered data
        if let Some((buf_idx, offset, total_len)) = this.recv_pending {
            let remaining = total_len - offset;
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(
                &this.transport.recv_buf(buf_idx)[offset..offset + n]
            );
            if n < remaining {
                this.recv_pending = Some((buf_idx, offset + n, total_len));
            } else {
                this.recv_pending = None;
                this.transport.repost_recv(buf_idx).map_err(io::Error::other)?;
            }
            return Poll::Ready(Ok(n));
        }

        // Phase 2: Poll transport for new recv completion
        let mut completions = [RecvCompletion::default(); 1];
        match this.transport.poll_recv(cx, &mut completions) {
            Poll::Pending => {
                if this.transport.poll_disconnect(cx) {
                    return Poll::Ready(Ok(0));  // EOF
                }
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                // FLUSH_ERR etc. — transport already marked qp_dead
                Poll::Ready(Ok(0))  // EOF
            }
            Poll::Ready(Ok(n)) if n > 0 => {
                let c = &completions[0];
                let copy_len = c.byte_len.min(buf.len());
                buf[..copy_len].copy_from_slice(
                    &this.transport.recv_buf(c.buf_idx)[..copy_len]
                );
                if copy_len < c.byte_len {
                    this.recv_pending = Some((c.buf_idx, copy_len, c.byte_len));
                } else {
                    this.transport.repost_recv(c.buf_idx).map_err(io::Error::other)?;
                }
                Poll::Ready(Ok(copy_len))
            }
            _ => Poll::Pending,
        }
    }
}

impl<T: Transport> AsyncWrite for AsyncRdmaStream<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8])
        -> Poll<io::Result<usize>>
    {
        let this = self.get_mut();

        if this.transport.is_qp_dead() {
            this.write_pending = None;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe, "connection closed"
            )));
        }

        // Post send if not in-flight
        if this.write_pending.is_none() {
            let n = this.transport.send_copy(buf).map_err(io::Error::other)?;
            if n == 0 {
                // All send buffers occupied. Wait for one to complete.
            } else {
                this.write_pending = Some(n);
            }
        }

        // If we haven't posted yet (buffers full), wait then retry
        if this.write_pending.is_none() {
            match this.transport.poll_send_completion(cx) {
                Poll::Pending => {
                    if this.transport.poll_disconnect(cx) {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe, "connection closed"
                        )));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
                Poll::Ready(Ok(())) => {
                    let n = this.transport.send_copy(buf).map_err(io::Error::other)?;
                    this.write_pending = Some(n);
                }
            }
        }
        let len = this.write_pending.unwrap();

        // Wait for THIS send's completion
        match this.transport.poll_send_completion(cx) {
            Poll::Pending => {
                if this.transport.poll_disconnect(cx) {
                    this.write_pending = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe, "connection closed"
                    )));
                }
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                this.write_pending = None;
                Poll::Ready(Err(io::Error::other(e)))
            }
            Poll::Ready(Ok(())) => {
                this.write_pending = None;
                Poll::Ready(Ok(len))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        // RDMA sends are flushed by the HCA — no application-level buffering.
        Poll::Ready(Ok(()))
    }

    /// Graceful close: drain pending send → DREQ → await DISCONNECTED.
    ///
    /// This 3-phase shutdown mirrors the actual AsyncRdmaStream::poll_close:
    /// 1. Wait for any in-flight send completion (don't disconnect mid-send)
    /// 2. Send DREQ to peer via transport.disconnect()
    /// 3. Await peer's DISCONNECTED event via CM fd polling
    ///
    /// If the QP is already dead (peer crashed), skip straight to done.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // Fast path: QP already dead — no graceful close possible.
        if this.transport.is_qp_dead() {
            this.write_pending = None;
            return Poll::Ready(Ok(()));
        }

        // Phase 1: Drain pending send completion.
        if this.write_pending.is_some() {
            match this.transport.poll_send_completion(cx) {
                Poll::Pending => {
                    if this.transport.poll_disconnect(cx) {
                        this.write_pending = None;
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(_) => {
                    this.write_pending = None;
                }
            }
        }

        // Phase 2: Send DREQ.
        this.transport.disconnect().map_err(io::Error::other)?;

        // Phase 3: Await DISCONNECTED from peer.
        if this.transport.poll_disconnect(cx) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl<T: Transport> Drop for AsyncRdmaStream<T> {
    fn drop(&mut self) {
        // Best-effort disconnect so the peer sees DREQ promptly.
        let _ = self.transport.disconnect();
    }
}

### RdmaUdpSocket (Datagram)

**Key architectural challenges** identified during design review:

1. **Multi-CQ waker starvation:** `poll_recv` iterates all connections' CQs, but the
   task waker is only registered with whichever CQ was last polled as Pending. Data on
   an earlier connection's CQ won't wake the task. Needs a combined waker mechanism.

2. **Shared mutable access for senders:** `UdpSender::poll_send` needs `&mut Transport`
   (for `poll_send_completion`), but connections are `Arc`-shared between socket and
   senders. Requires per-transport `Mutex` or redesign.

3. **Send buffer contention:** Multiple `UdpSender`s must use different send buffer
   indices. Requires a buffer allocation strategy (atomic counter, per-sender slot, etc.)

4. **Async accept in sync poll:** RDMA CM accept is multi-step async. Need an
   in-progress accept state machine driven across `poll_recv` invocations.

5. **Dead connection cleanup:** Disconnected transports return FLUSH_ERR forever.
   Must detect and remove from the connection map.

```rust
pub struct RdmaUdpSocket<T: Transport> {
    listener: AsyncCmListener,
    /// Shared connection map — same Arc is given to senders.
    /// poll_accept inserts new connections; senders read on every poll_send.
    connections: Arc<std::sync::RwLock<HashMap<SocketAddr, Arc<Mutex<T>>>>>,
    local_addr: SocketAddr,
    /// In-progress accept future, driven across poll_recv invocations.
    pending_accept: Option<Pin<Box<dyn Future<Output = Result<(SocketAddr, T)>> + Send>>>,
    /// Factory function to create transports from accepted connections.
    /// Called by poll_accept when a new CONNECT_REQUEST arrives.
    accept_factory: AcceptFactory<T>,
}

/// Per-task sender. Shares the connection map via Arc<RwLock<>>.
/// New connections accepted after sender creation are visible.
pub struct RdmaUdpSender<T: Transport> {
    connections: Arc<std::sync::RwLock<HashMap<SocketAddr, Arc<Mutex<T>>>>>,
}

/// Factory that produces transports from incoming RDMA CM connections.
/// Receives the conn_id (CmId) from get_request + config.
type AcceptFactory<T> = Box<dyn Fn(CmId, TransportConfig) ->
    Pin<Box<dyn Future<Output = Result<(SocketAddr, T)>> + Send>> + Send + Sync>;

impl<T: Transport> RdmaUdpSocket<T> {
    /// Drive the accept state machine. Non-blocking — called at the
    /// start of every poll_recv.
    ///
    /// State machine: `pending_accept` is a single future that encompasses
    /// BOTH get_request + factory setup. When None, we start a new one.
    ///
    /// ```text
    /// None ──► create future: listener.get_request() → factory(conn_id)
    ///          └─ pending_accept = Some(future)
    ///
    /// Some(future) ──► poll the combined future
    ///                   ├─ Pending → return (waker registered on CM fd)
    ///                   └─ Ready(Ok(addr, transport))
    ///                        └─ insert into connections map
    ///                           └─ pending_accept = None
    ///                              └─ try accepting another (loop)
    /// ```
    fn poll_accept(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        loop {
            if let Some(fut) = &mut self.pending_accept {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Ok(()),
                    Poll::Ready(Ok((addr, transport))) => {
                        self.connections.write().unwrap()
                            .insert(addr, Arc::new(Mutex::new(transport)));
                        self.pending_accept = None;
                        // Loop to start accepting another immediately.
                    }
                    Poll::Ready(Err(e)) => {
                        eprintln!("RDMA accept failed: {e}");
                        self.pending_accept = None;
                        return Ok(());
                    }
                }
            } else {
                // Start a new accept future that does:
                //   1. listener.get_request().await → CmId
                //   2. factory(conn_id, config).await → (SocketAddr, T)
                // Both steps are inside one pinned future.
                // NOTE: This captures a reference to self.listener which is
                // &AsyncCmListener. In practice, the listener would be wrapped
                // in Arc or the future would own a clone of the listener handle.
                let fut = Self::make_accept_future(
                    &self.listener, &self.accept_factory
                );
                self.pending_accept = Some(fut);
                // Loop to poll the newly created future immediately.
            }
        }
    }

    fn make_accept_future(
        listener: &AsyncCmListener,
        factory: &AcceptFactory<T>,
    ) -> Pin<Box<dyn Future<Output = Result<(SocketAddr, T)>> + Send>> {
        // In implementation, this would capture the listener by Arc
        // and the factory by Arc to avoid lifetime issues. Sketch:
        //
        // let listener = Arc::clone(&listener);
        // let factory = Arc::clone(&factory);
        // Box::pin(async move {
        //     let conn_id = listener.get_request().await?;
        //     let config = TransportConfig::datagram();
        //     factory(conn_id, config).await
        // })
        todo!("Implementation detail — requires Arc<AsyncCmListener>")
    }
}

impl<T: Transport> AsyncUdpSocket for RdmaUdpSocket<T> {
    fn poll_recv(
        &mut self, cx: &mut Context,
        bufs: &mut [IoSliceMut], meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut count = 0;

        // Accept new connections (non-blocking state machine)
        self.poll_accept(cx)?;

        // Collect dead connections for cleanup
        let mut dead_addrs = Vec::new();

        // Poll all transports' recv CQs.
        // WAKER NOTE: Each poll_recv(cx, ...) registers cx.waker() with
        // that transport's CQ fd. We're registered with ALL CQs — any
        // becoming ready wakes this task.
        let connections = self.connections.read().unwrap();
        for (addr, transport_arc) in connections.iter() {
            let mut transport = transport_arc.lock().unwrap();

            if transport.is_qp_dead() {
                dead_addrs.push(*addr);
                continue;
            }

            let mut completions = [RecvCompletion::default(); 8];
            while count < bufs.len() {
                match transport.poll_recv(cx, &mut completions) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        for i in 0..n {
                            if count >= bufs.len() { break; }
                            let c = &completions[i];

                            bufs[count][..c.byte_len].copy_from_slice(
                                &transport.recv_buf(c.buf_idx)[..c.byte_len]
                            );
                            meta[count] = RecvMeta {
                                addr: *addr, len: c.byte_len, stride: c.byte_len,
                                ecn: None, dst_ip: Some(self.local_addr.ip()),
                            };

                            transport.repost_recv(c.buf_idx)?;
                            count += 1;
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        // Transport marks itself dead on FLUSH_ERR.
                        dead_addrs.push(*addr);
                        break;
                    }
                    _ => break,
                }
            }
        }

        // Clean up dead connections (need write lock)
        drop(connections);  // release read lock first
        if !dead_addrs.is_empty() {
            let mut connections = self.connections.write().unwrap();
            for addr in dead_addrs {
                connections.remove(&addr);
            }
        }

        if count > 0 { Poll::Ready(Ok(count)) } else { Poll::Pending }
    }

    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(RdmaUdpSender {
            // Share the SAME connection map — new connections visible to senders.
            connections: Arc::clone(&self.connections),
        })
    }
}

impl<T: Transport + 'static> UdpSender for RdmaUdpSender<T> {
    fn poll_send(
        self: Pin<&mut Self>, transmit: &Transmit, cx: &mut Context,
    ) -> Poll<io::Result<()>> {
        let connections = self.connections.read().unwrap();
        let transport_arc = connections.get(&transmit.destination)
            .ok_or_else(|| io::Error::new(
                io::ErrorKind::NotConnected, "no RDMA connection"
            ))?;
        let mut transport = transport_arc.lock().unwrap();

        // Try to send FIRST — don't wait for a completion that doesn't exist.
        // On first send, no previous send is in-flight; poll_send_completion
        // would return Pending forever (empty CQ).
        let n = transport.send_copy(transmit.contents)
            .map_err(io::Error::other)?;
        if n > 0 {
            // Sent successfully. Don't wait for THIS send's completion —
            // it'll be reaped on the next poll_send if buffers are full.
            return Poll::Ready(Ok(()));
        }

        // send_copy returned 0 — all send buffers occupied.
        // Wait for one to complete, then retry.
        ready!(transport.poll_send_completion(cx)
            .map_err(|e| io::Error::other(e)))?;

        // Retry — at least one buffer is now free.
        transport.send_copy(transmit.contents)
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(()))
    }
}
```

## What Each Layer Is Responsible For

| Concern | RdmaTransport (shared) | AsyncRdmaStream (stream) | RdmaUdpSocket (datagram) |
|---------|----------------------|------------------------|------------------------|
| QP creation | ✓ | — | — |
| Buffer/ring allocation | ✓ (configurable) | — | — |
| MR registration | ✓ | — | — |
| Pre-post recv bufs | ✓ | — | — |
| CM connect/accept | ✓ | — | — |
| CQ polling | ✓ (`poll_send_completion` / `poll_recv`) | Calls transport | Calls transport |
| WC status → Err() | ✓ (FLUSH_ERR → Err, mark qp_dead) | Handles Err() → EOF/BrokenPipe | Handles Err() → remove dead |
| Send buffer management | ✓ (internal slot allocation) | Just calls `send_copy(data)` | Just calls `send_copy(data)` |
| Disconnect detection | ✓ (`poll_disconnect`) | Calls transport | Checks `is_qp_dead()` |
| Buffer repost | ✓ (`repost_recv`) | Calls after partial consume | Calls immediately |
| Partial read buffering | — | ✓ (`recv_pending`) | — (always full packet) |
| Graceful close | — | ✓ (`poll_close` + drain) | — (not needed for QUIC) |
| Multi-peer management | — | — (1 transport = 1 stream) | ✓ (`HashMap<Addr, Arc<Mutex<Transport>>>`) |
| Dead connection cleanup | — | — | ✓ (remove on Err) |
| Connection map | — | — | ✓ |
| Batch recv across peers | — | — | ✓ |

## Key Differences in How They Use RdmaTransport

### Buffer Lifecycle

```
Stream (AsyncRdmaStream):
  recv completion → copy partial to user buf → hold buffer → more reads → repost when done
  Very long buffer hold time (user controls read pace)

Datagram (RdmaUdpSocket):
  recv completion → copy full packet to Quinn's buf → repost immediately
  Very short buffer hold time (copy and release)
```

### Send Pattern

```
Stream:
  1 send buffer, sequential: write → wait completion → write → ...
  Single writer assumed (AsyncWrite is &mut self)

Datagram:
  N send buffers, round-robin: multiple UdpSenders can post concurrently
  Need send buffer index allocation (simple counter or pool)
```

### Config Differences

| Parameter | Stream | Datagram | Why |
|-----------|--------|----------|-----|
| `buf_size` | 64 KB | 1.5 KB (MTU) | Stream needs throughput; datagram needs many small buffers |
| `num_recv_bufs` | 8 | 64 | Stream reads frequently; datagram must absorb bursts |
| `num_send_bufs` | 1 | 4 | Stream has single writer; datagram has multiple senders |
| `max_inline_data` | 0 | 64 | Small QUIC packets benefit from inline |

## Migration Path

### Phase 1: Transport Trait + RdmaTransport
1. Create `rdma-io/src/transport.rs` with `Transport` trait, `RecvCompletion`
2. Create `rdma-io/src/rdma_transport.rs` with `RdmaTransport` + `TransportConfig`
3. Implement `Transport for RdmaTransport` (move QP setup, buffer alloc, CQ polling,
   WC→completion conversion, disconnect detection from `async_stream.rs`)
4. Rewrite `AsyncRdmaStream<T: Transport>` to use trait methods
5. Add `AsyncRdmaStream<RdmaTransport>::connect()` convenience constructor
6. All existing tests must pass unchanged

### Phase 2: Build RdmaUdpSocket
1. Create `rdma-io/src/udp_socket.rs` (or new `quinn-rdma` crate)
2. Implement `AsyncUdpSocket` + `UdpSender` using `T: Transport`
3. Add multi-transport connection map with `Arc<Mutex<T>>`
4. Test with Quinn

### Phase 3: Optimize
1. Inline data for small packets
2. Shared Receive Queue (SRQ) across QPs for memory efficiency
3. Optional UD QP mode for true UDP semantics
4. (Future) `RdmaRingTransport` implementation

## Open Design Questions

1. **Send buffer allocation for multiple senders:** `UdpSender::poll_send` is called from
   multiple tasks. Each needs a send buffer. **Resolved:** Use sender_id % num_send_bufs for
   round-robin allocation. Each sender gets a dedicated buffer index. num_send_bufs must be ≥
   max concurrent senders.

2. **Should RdmaTransport own the CqPollState?** Currently yes (simplest). But if
   `AsyncRdmaStream` and `RdmaUdpSocket` need to compose `poll_recv` with
   other poll operations, they might want to own the state. Could move to `&mut CqPollState`
   parameter like `AsyncQp::poll_recv_cq` does today.

3. **SRQ for multi-peer datagram:** With 20 peers × 64 recv buffers = 1280 buffers.
   A Shared Receive Queue pools recv buffers across all QPs, reducing total to ~128.
   This is a significant optimization but changes the buffer management model.

4. **Trait vs struct:** **Resolved:** `Transport` trait is the core abstraction from day one.
   Generics (`T: Transport`) monomorphize → zero vtable overhead. Enables mock transports for
   testing and future Ring transport as a drop-in replacement.

5. **Multi-CQ waker in poll_recv:** **Validated.** Each `poll_recv(cx, ...)` registers
   cx.waker() with that CQ's own AsyncFd. Since different connections have different
   `AsyncFd` instances (different fds), each independently stores a waker clone. Tokio's
   `poll_read_ready` docs confirm: "store a clone of the Waker." The "only last waker"
   limitation is per-instance, not cross-instance. Any CQ becoming ready wakes the task.

6. **Holding Mutex\<Transport\> across Poll::Pending:** **Resolved — no deadlock, brief
   contention only.** Analysis:
   - `poll_send` acquires one lock. If `poll_send_completion` returns Pending, `ready!()`
     causes the function to return → MutexGuard dropped. Lock released. ✓
   - `poll_recv` acquires locks one-at-a-time (sequential iteration). If `poll_recv` returns
     Pending, `break` exits inner loop → MutexGuard dropped at `for` body end. ✓
   - No function ever holds two locks simultaneously → circular wait impossible → no deadlock.
   - Lock hold time: non-blocking CQ poll (memory-mapped read, nanoseconds) + memcpy +
     ibv_post_send/recv. Total ~1-50µs per batch.
   - `std::sync::Mutex` is correct choice: brief, non-async holds. `tokio::Mutex` would be
     wrong (designed for holds across `.await` points, which we don't have).

## Future: Ring Buffer Transport

A ring buffer transport (`RdmaRingTransport`) would implement the same `Transport` trait using
RDMA Write + Immediate Data instead of Send/Recv — the same approach as msquic and rsocket.
Since consumers are already generic over `T: Transport`, this is a **drop-in replacement**:

```rust
// Just change the transport type — consumer code is identical
let transport = RdmaRingTransport::connect(addr, RingConfig::default()).await?;
let stream = AsyncRdmaStream::new(transport);  // same AsyncRdmaStream<T>
```

### How RdmaRingTransport Differs Internally

The trait API is identical — the difference is entirely in the implementation:

| Operation | `RdmaTransport` (Send/Recv) | `RdmaRingTransport` (Write+Ring) |
|-----------|---------------------------|----------------------------------|
| `send_copy()` | Copy → MR[round_robin_idx], `ibv_post_send(SEND)` | Check remote credit → copy → send_ring[tail], `ibv_post_send(WRITE_WITH_IMM)` |
| `poll_recv()` | Poll RecvCQ, convert WC → `RecvCompletion` | Poll RecvCQ for doorbells + drain stash, decode immediate → `RecvCompletion { virtual_idx, byte_len }` |
| `recv_buf(idx)` | Return `recv_mrs[idx]` slice | Return `recv_ring[offset..offset+len]` via virtual_idx_map |
| `repost_recv(idx)` | `ibv_post_recv` on buffer slot | Advance ring Head (with out-of-order tracking), repost doorbell, send credit update |
| `poll_send_completion()` | Poll SendCQ, return `Ok(())` when one completes | Poll SendCQ + release send ring slot, return `Ok(())` |
| Setup | Create QP, alloc MRs | Create QP, alloc rings, exchange tokens (VA + rkey + capacity) |

### RdmaRingTransport Sketch

```rust
pub struct RdmaRingTransport {
    // CQ poll state and connection flags (no RDMA teardown)
    send_cq_state: CqPollState,
    recv_cq_state: CqPollState,
    disconnected: bool,
    qp_dead: bool,
    peer_disconnected: bool,

    // Virtual buffer index mapping: ring offsets → discrete indices
    virtual_idx_map: HashMap<usize, (usize, usize)>,  // virt_idx → (offset, len)
    next_virt_idx: usize,

    // Stashed completions: doorbells polled from CQ but not yet returned
    // to caller (overflow from poll_recv output buffer).
    // CRITICAL: losing doorbell immediate data = losing data location in ring.
    recv_stash: VecDeque<RecvCompletion>,

    // RDMA data-path resources
    qp: AsyncQp,

    // Ring buffers (replace discrete recv_bufs)
    send_ring: RingBuffer,       // Local staging → RDMA Write source
    recv_ring: RingBuffer,       // Landing zone for remote writes
    remote_ring: RemoteRingInfo, // Peer's recv ring VA + rkey + capacity

    // Doorbell recv buffers (small — just for immediate data notification)
    doorbell_bufs: Vec<OwnedMemoryRegion>,

    // Credit tracking: sender's estimate of remote ring free space.
    // Decremented on send_copy, incremented when peer acks consumption.
    // Without this, sender can overwrite unconsumed data (silent corruption).
    remote_credits: usize,

    // Completion tracking for out-of-order release
    send_completions: CompletionTracker,
    recv_completions: CompletionTracker,

    // CM resources — drop order: AsyncFd → CmId → EventChannel
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,
}

struct RingBuffer {
    mr: OwnedMemoryRegion,   // Registered memory for the entire ring
    capacity: usize,
    head: usize,             // Consumer position
    tail: usize,             // Producer position
}

struct RemoteRingInfo {
    addr: u64,               // Remote virtual address
    rkey: u32,               // Remote memory key
    capacity: usize,
    head: usize,             // Tracked remotely (via RDMA Read or credits)
    tail: usize,
}

struct CompletionTracker {
    pending: HashMap<usize, usize>,  // offset → length for out-of-order completions
}
```

### Trait Compatibility: The buf_idx Mapping

The key challenge is that the `Transport` trait uses `buf_idx` (discrete buffer index) for both
`recv_buf()` and `repost_recv()`. The ring transport doesn't have discrete buffers — it has
a contiguous ring. The solution is a **virtual buffer index**:

```rust
impl Transport for RdmaRingTransport {
    fn send_copy(&mut self, data: &[u8]) -> Result<usize> {
        // Check remote credits — can we write to the peer's ring?
        if self.remote_credits == 0 {
            return Ok(0);  // Ring full — caller should retry after completions
        }
        let len = data.len().min(self.send_ring.available());
        if len == 0 {
            return Ok(0);  // Local send ring full too
        }

        // Reserve local send ring slot
        let local_offset = self.send_ring.alloc(len);
        self.send_ring.mr.as_mut_slice()[local_offset..local_offset + len]
            .copy_from_slice(&data[..len]);

        // Reserve remote ring slot
        let remote_offset = self.remote_ring.alloc(len);
        self.remote_credits -= 1;

        // Encode immediate: (offset << 16) | length
        // NOTE: limits ring to 64KB. For larger rings, use length-only
        // encoding + RDMA Read for offset (msquic large ring mode).
        let imm = ((remote_offset as u32) << 16) | (len as u32);

        // RDMA Write With Immediate → data lands in remote ring,
        // doorbell generates recv completion on remote CQ
        self.qp.post_write_with_imm(
            &self.send_ring.mr, local_offset, len,
            &self.remote_ring, remote_offset, imm,
        )?;
        Ok(len)
    }

    fn poll_recv(
        &mut self, cx: &mut Context, completions: &mut [RecvCompletion],
    ) -> Poll<Result<usize>> {
        let mut count = 0;

        // Phase 1: Drain stashed completions first (from previous overflow)
        while count < completions.len() && !self.recv_stash.is_empty() {
            completions[count] = self.recv_stash.pop_front().unwrap();
            count += 1;
        }
        if count > 0 {
            return Poll::Ready(Ok(count));
        }

        // Phase 2: Poll CQ for new doorbells
        let mut wc_buf = [WorkCompletion::default(); 16];
        let n = ready!(self.qp.poll_recv_cq(
            cx, &mut self.recv_cq_state, &mut wc_buf
        ))?;

        // Phase 3: Process ALL doorbells — stash any that don't fit
        for i in 0..n {
            if !wc_buf[i].is_success() {
                self.qp_dead = true;
                return Poll::Ready(Err(Error::from_wc_status(wc_buf[i].status())));
            }

            let imm = wc_buf[i].imm_data();
            let offset = (imm >> 16) as usize;
            let length = (imm & 0xFFFF) as usize;
            let virt_idx = self.next_virt_idx;
            self.next_virt_idx += 1;
            self.virtual_idx_map.insert(virt_idx, (offset, length));

            let completion = RecvCompletion {
                buf_idx: virt_idx,
                byte_len: length,
            };

            if count < completions.len() {
                completions[count] = completion;
                count += 1;
            } else {
                // STASH excess — can't lose doorbell immediate data!
                self.recv_stash.push_back(completion);
            }
        }

        if count > 0 {
            Poll::Ready(Ok(count))
        } else {
            Poll::Pending
        }
    }

    fn recv_buf(&self, buf_idx: usize) -> &[u8] {
        let (offset, length) = self.virtual_idx_map[&buf_idx];
        &self.recv_ring.mr.as_slice()[offset..offset + length]
    }

    fn repost_recv(&mut self, buf_idx: usize) -> Result<()> {
        let (offset, length) = self.virtual_idx_map.remove(&buf_idx)
            .ok_or(Error::InvalidArg("unknown buf_idx".into()))?;
        // Advance ring head (with out-of-order completion tracking)
        self.recv_ring.release(offset, length);
        // Repost a doorbell recv (not data recv — just notification slot)
        let next_doorbell = self.next_doorbell_idx();
        self.qp.post_recv_buffer(
            &self.doorbell_bufs[next_doorbell], next_doorbell as u64
        )?;
        // Send credit update to peer so they know we freed ring space.
        // Could be piggybacked on next data send or sent as a dedicated
        // 0-byte Send with immediate data encoding the credit count.
        self.send_credit_update()?;
        Ok(())
    }

    // ... disconnect, local_addr, peer_addr, is_qp_dead,
    //     poll_disconnect, poll_send_completion — same pattern as RdmaTransport
}
```

This mapping is invisible to consumers — `AsyncRdmaStream` and `RdmaUdpSocket` use `buf_idx`
identically regardless of whether the transport is Send/Recv or Write+Ring.

### Consumer Code: API Unchanged, Behavior Differs

The trait abstraction means `AsyncRdmaStream` and `RdmaUdpSocket` compile and run with
either transport — zero code changes:

```rust
// Stream works with either transport — zero code changes
pub struct AsyncRdmaStream<T: Transport> {
    transport: T,
    recv_pending: Option<(usize, usize, usize)>,
    write_pending: Option<usize>,
}

// Datagram works with either transport — zero code changes
pub struct RdmaUdpSocket<T: Transport> {
    listener: AsyncCmListener,
    connections: HashMap<SocketAddr, T>,
    local_addr: SocketAddr,
}
```

Usage at construction time:

```rust
// Send/Recv transport (simple, portable)
let stream = AsyncRdmaStream::new(RdmaTransport::connect(addr, config).await?);

// Ring buffer transport (fewer copies, higher throughput)
let stream = AsyncRdmaStream::new(RdmaRingTransport::connect(addr, config).await?);

// Both implement the same AsyncRead/AsyncWrite — caller doesn't know the difference
```

**⚠ Behavioral differences to be aware of:**

| Behavior | Send/Recv | Ring |
|----------|-----------|------|
| `send_copy` backpressure | Always accepts (hardware RNR) | May return `Ok(0)` when ring full |
| `recv_pending` (partial read) | Blocks 1 of N independent buffers | Pins ring Head → **app-level HOL blocking** |
| Credit management | None needed (hardware) | Must send credit updates on `repost_recv` |

The **datagram** consumer is unaffected — it copies and reposts immediately, so ring Head
advances promptly. The **stream** consumer's partial reads hold ring space, creating the
same app-level HOL blocking documented in the comparison doc. This is inherent to the
ring architecture, not a trait design flaw — but stream workloads should prefer Send/Recv.

### What the Ring Transport Adds Under the Hood

Token exchange during connection setup:

```
  Client                                Server
    │ Connect(QP)                         │ Accept(QP)
    │ Bind MW to recv_ring                │ Bind MW to recv_ring
    │ Post doorbell recv                  │ Post doorbell recv
    │                                     │
    │ Send(my_ring_VA + rkey + capacity)  │
    │ ─────────────────────────────────►  │
    │                                     │ Parse → store as remote_ring
    │  ◄─────────────────────────────────  │
    │ Parse → store as remote_ring        │ Send(my_ring_VA + rkey + capacity)
    │                                     │
    │ Ready ◄─────────────────────────────► Ready
```

Send path:

```
send_copy(data):
  1. Check remote_credits > 0 — if 0, return Ok(0) (ring full)
  2. Alloc local send_ring slot at tail
  3. Copy data → send_ring[tail]
  4. Alloc remote ring slot, decrement remote_credits
  5. Encode immediate: (remote_offset << 16) | length
     NOTE: 16-bit encoding limits ring to 64KB. For larger rings,
     use length-only encoding + separate offset channel (msquic mode).
  6. ibv_post_send(WRITE_WITH_IMM, local=send_ring[slot], remote=remote_ring[slot], imm)
  7. Return Ok(len)

poll_send_completion:
  1. Poll SendCQ
  2. For each completion: release send_ring slot (advance head)
  3. Return Ok(()) when at least one completed
```

Recv path:

```
poll_recv:
  1. Drain recv_stash first (overflow from previous poll)
  2. Poll RecvCQ for Write-With-Immediate doorbells
  3. For each doorbell: decode immediate → offset + length
  4. Data already in recv_ring[offset] — zero-copy on receiver!
  5. Allocate virtual buf_idx, store in virtual_idx_map
  6. If output buffer full, stash remainder in recv_stash
  7. Return completions

repost_recv(idx):
  1. Remove virtual_idx_map entry → (offset, length)
  2. Advance recv_ring head (with out-of-order completion tracking)
  3. Repost doorbell recv buffer (not data — just notification slot)
  4. Send credit update to peer (piggyback or dedicated message)
```

### Trade-offs: When to Use Which

```
                     RdmaTransport              RdmaRingTransport
                     (Send/Recv)                (Write+Ring)
                ┌────────────────────┐    ┌────────────────────────┐
  Simplicity    │ ████████████████   │    │ ███                    │
  Portability   │ ████████████████   │    │ ██████████             │
  Recv copies   │ ████ (2 copies)    │    │ ████████████████ (1)   │
  Throughput    │ ████████████       │    │ ████████████████       │
  Latency       │ ██████████████     │    │ ████████████████       │
  Code size     │ ████████████████   │    │ ██████                 │
  Safety        │ ████████████████   │    │ ████████ (MW needed)   │
  Stream HOL    │ ████████████████   │    │ ████ (ring Head blocks)│
                └────────────────────┘    └────────────────────────┘

  Use RdmaTransport:     QUIC, RPC, byte streams, many peers, early development
  Use RdmaRingTransport: Datagram bulk transfer, max throughput, few peers
```

Both transports implement the same `Transport` trait. Consumers pick at construction time.
Switching requires no code changes in `AsyncRdmaStream` or `RdmaUdpSocket`.

## Design Review Findings (2026-03-14)

Review compared this design against the actual `AsyncRdmaStream` implementation
(`async_stream.rs`, 692 lines) and `quinn-rdma.md`. The following flaws were identified
and fixed inline:

### Fixed in This Revision

| # | Severity | Issue | Fix |
|---|----------|-------|-----|
| 1 | **Critical** | Drop order: `event_channel` before `cm_async_fd` → use-after-close of fd | Reordered struct fields; `cm_async_fd` now drops before `event_channel` |
| 2 | **Critical** | No WC status check in poll_read/poll_write sketches — garbage data on FLUSH_ERR | WC status checking moved into transport; consumers get Err() or valid completions |
| 3 | **Critical** | `send_copy(&self)` needs mutation for `as_mut_slice()` | Changed to `&mut self` |
| 4 | **High** | `poll_disconnect` was passive check — no waker registration on CM fd | Renamed to `poll_disconnect` + registers waker via `poll_read_ready(cx)` |
| 5 | **High** | All UdpSenders used `send_copy(0, ...)` — data race with concurrent senders | Removed buf_idx from send_copy; transport manages buffer selection internally |
| 6 | **High** | UdpSender needed `&mut Transport` but connections were bare Arc | Changed to `Arc<Mutex<RdmaTransport>>` per transport |
| 7 | **High** | `peer_gone` conflated disconnect event with QP death | Split into `peer_disconnected` + `qp_dead` with distinct semantics |
| 8 | **High** | `send_buf_mut` / `post_send` / `num_send_bufs` incompatible with Ring | Removed; only `send_copy` exposed. Transport manages send buffers internally |
| 9 | **High** | Ring `poll_recv` dropped excess doorbells — lost data location | Ring must stash overflow in `recv_stash: VecDeque<RecvCompletion>` |
| 10 | **High** | Ring `send_copy` had no backpressure — could overwrite unread ring data | Added `remote_credits` tracking; `send_copy` returns `Ok(0)` when ring full |
| 11 | **Medium** | Transport trait leaked `WorkCompletion` | Introduced `RecvCompletion` / `SendCompletion`; WC stays internal to transport |
| 12 | **Medium** | `num_recv_bufs`/`buf_size` meaningless for Ring | Replaced with `max_send_size()` and `max_recv_capacity()` |
| 13 | **Medium** | "Consumer Code: Unchanged" claim misleading — HOL behavior differs | Added behavioral differences table; stream partial reads create Ring HOL |
| 14 | **Medium** | No dead connection cleanup in datagram poll_recv | Added dead_addrs collection and removal |
| 15 | **Medium** | No factory method on Transport trait for generic construction | Added recommendation: consumers take pre-constructed `T` via `::new(transport)` |
| 16 | **Medium** | RdmaRingTransport had same drop order bug as original RdmaTransport | Fixed field ordering |
| 17 | **Medium** | Ring `repost_recv` needed `&mut self` but trait had `&self` | Changed trait to `&mut self` |
| 18 | **Low** | Datagram poll_recv polled 1 WC at a time | Changed to batch of 8 |
| 19 | **Critical** | `accept_factory` didn't receive `conn_id` — and `poll_get_request` doesn't exist | Factory takes `(CmId, TransportConfig)`. Accept future wraps both `get_request()` and factory in one pinned future |
| 20 | **Critical** | `poll_send` waited BEFORE sending → deadlocks on first send (empty CQ) | Reversed: send first, wait only if `send_copy` returns 0 (buffers full) |
| 21 | **High** | Sender had stale HashMap snapshot — new connections invisible | Changed to `Arc<RwLock<HashMap>>` shared between socket and all senders |
| 22 | **High** | `send_copy` round-robin without in-flight tracking → data corruption | Added `send_in_flight: Vec<bool>` — checks before reusing buffer, returns `Ok(0)` when all occupied |
| 23 | **High** | `?` on `Poll<Result>` doesn't compile (6+ places) | Replaced with explicit `match Poll::Ready(Ok/Err) / Pending` throughout |
| 24 | **Medium** | Missing `Unpin` bound on `AsyncRdmaStream<T>` | Added `impl<T: Transport> Unpin for AsyncRdmaStream<T>` |
| 25 | **Medium** | `Error::from_wc_status` doesn't exist in codebase | Replaced with `crate::Error::Verbs(io::Error::other(format!(...)))` |
| 26 | **Low** | `AtomicUsize` unnecessary for `next_send_idx` (`&mut self` = exclusive) | Changed to plain `usize` |

### Identified but Not Yet Resolved

| # | Issue | Notes |
|---|-------|-------|
| A | ~~Async accept in sync poll~~ | **Resolved.** `poll_accept` drives a pinned boxed future across `poll_recv` invocations. Loop accepts multiple queued connections. Errors logged, not propagated. |
| B | ~~Multi-CQ waker correctness~~ | **Resolved — design is correct.** Each transport has its own `AsyncFd` (different fd, different instance). Tokio stores a waker clone per `AsyncFd`. The "only last waker" limitation applies within ONE `AsyncFd`, not across different instances. When iterating N connections, all N `AsyncFd`s independently hold a waker clone — any becoming ready wakes the task. Confirmed via tokio source: `poll_read_ready` docs say "store a clone of the Waker" per instance. |
| C | ~~Mutex contention between poll_recv and poll_send~~ | **Resolved — no deadlock, brief contention.** Neither operation holds two locks simultaneously (no circular wait). Lock released on Pending (MutexGuard dropped on return/break). Hold time ~1-50µs (non-blocking CQ poll + memcpy). `std::sync::Mutex` is correct choice. |
| D | **Ring credit protocol details** | **Deferred** to Ring transport implementation (Phase 3). Does not affect Send/Recv transport or any Phase 1-2 work. |
| E | **Ring 64KB limit** | **Deferred** to Ring transport implementation (Phase 3). Internal to `RdmaRingTransport`; trait and consumers unaffected. |

## References

- [docs/design/quinn-rdma.md](quinn-rdma.md) — Quinn RDMA integration design
- [docs/design/rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way design comparison
- [rdma-io/src/async_stream.rs](../../rdma-io/src/async_stream.rs) — Current AsyncRdmaStream
- [rdma-io/src/async_qp.rs](../../rdma-io/src/async_qp.rs) — AsyncQp primitives
