# Quinn over RDMA — Design Document

**Status:** Research / Proposal  
**Date:** 2026-03-14  
**Dependencies:** `quinn`, `quinn-proto`, `quinn-udp`, `rdma-io`

## Motivation

MsQuic PR #5113 demonstrates QUIC-over-RDMA using RDMA Write into ring buffers, requiring ~6000 lines
of C to implement the ring buffer protocol, token exchange, 13-state connection machine, and IOCP
integration. Quinn's architecture is fundamentally different and much more amenable to RDMA
integration: the `AsyncUdpSocket` trait provides a clean, public abstraction point where we can
substitute RDMA transport **without modifying Quinn itself**.

This document applies the learnings from msquic-RDMA to design a Quinn RDMA datapath using our
existing `rdma-io` crate.

## Quinn's Architecture

Quinn separates protocol logic from I/O via two crates:

```
  Application (streams, datagrams)
       │
  quinn-proto (pure state machine — no I/O)
    │ Endpoint::handle(BytesMut) → DatagramEvent
    │ Connection::poll_transmit() → Transmit
       │
  quinn (async I/O driver)
    │ EndpointDriver polls AsyncUdpSocket
    │ Routes packets to/from proto::Endpoint
       │
  Box<dyn AsyncUdpSocket>     ← SUBSTITUTION POINT
       │
  quinn-udp (default: kernel UDP)
```

**Key insight:** `quinn-proto` never touches sockets. It consumes raw bytes + metadata and
produces `Transmit` structs. The `quinn` crate's `EndpointDriver` bridges between the socket
and the protocol via two traits.

### The Socket Abstraction

```rust
/// The socket trait we need to implement
pub trait AsyncUdpSocket: Send + Sync + Debug + 'static {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>>;
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
    fn max_receive_segments(&self) -> usize { 1 }
    fn may_fragment(&self) -> bool { true }
}

/// Per-task sender (multiple senders per socket)
pub trait UdpSender: Send + Sync + Debug + 'static {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>>;
    fn max_transmit_segments(&self) -> usize { 1 }
}
```

### Data Types

```rust
/// Outgoing packet
pub struct Transmit<'a> {
    pub destination: SocketAddr,    // Remote peer address
    pub ecn: Option<EcnCodepoint>,  // Congestion notification
    pub contents: &'a [u8],         // QUIC packet bytes
    pub segment_size: Option<usize>,// GSO segment size (None = single)
    pub src_ip: Option<IpAddr>,     // Source IP
}

/// Received packet metadata
pub struct RecvMeta {
    pub addr: SocketAddr,           // Source address
    pub len: usize,                 // Total bytes received
    pub stride: usize,              // Per-datagram size (GRO)
    pub ecn: Option<EcnCodepoint>,  // ECN bits
    pub dst_ip: Option<IpAddr>,     // Destination IP
}
```

## How Quinn's Recv Loop Works

The `EndpointDriver` future runs a loop:

```
loop {
    socket.poll_recv(cx, &mut iovs, &mut metas)?;
    for (meta, buf) in metas.zip(iovs) {
        // Split by stride (GRO coalesced datagrams)
        while !data.is_empty() {
            let pkt = data.split_to(meta.stride.min(data.len()));
            match proto_endpoint.handle(now, meta.addr, meta.dst_ip, meta.ecn, pkt, &mut resp) {
                DatagramEvent::NewConnection(incoming) => ...,
                DatagramEvent::ConnectionEvent(handle, event) => ...,
                DatagramEvent::Response(transmit) => ...,
            }
        }
    }
}
```

Each `poll_recv` returns up to `BATCH_SIZE` (currently 32 on Linux) datagrams.
Quinn allocates a large recv buffer: `max_udp_payload × max_recv_segments × BATCH_SIZE`.

## Proposed Design: `quinn-rdma` Crate

### Strategy: Send/Recv via Transport Trait

Unlike msquic which uses RDMA Write + ring buffers (one-sided), we implement the socket traits
using the **`Transport` trait** from [rdma-transport-layer.md](rdma-transport-layer.md). The
initial implementation uses `RdmaTransport` (Send/Recv two-sided), but the generic design
allows future Ring buffer transport as a drop-in replacement.

- No ring buffer protocol, no token exchange, no multi-state connection handshake
- No remote memory exposure (no Memory Windows / rkeys needed)
- Directly maps to Quinn's `poll_recv` / `poll_send` model
- Shares the same `Transport` trait as `AsyncRdmaStream<T>` — tested, reviewed code

### Architecture: Transport Trait Layer

Both `AsyncRdmaStream<T>` and `RdmaUdpSocket<T>` are generic over `T: Transport`:

```
                 ┌───────────────────────────────────────────┐
                 │          Transport trait (core)            │
                 │  send_copy · poll_recv/send_completions   │
                 │  recv_buf · repost_recv · disconnect      │
                 │  poll_disconnect                           │
                 └─────────┬─────────────────┬───────────────┘
                           │                 │
          ┌────────────────▼──────┐ ┌────────▼──────────────────┐
          │ AsyncRdmaStream<T>    │ │ RdmaUdpSocket<T> (new)    │
          │ (byte stream)         │ │ (datagram socket)         │
          │                       │ │                           │
          │ • AsyncRead/Write     │ │ • AsyncUdpSocket/UdpSender│
          │ • Partial reads       │ │ • 1 recv = 1 packet       │
          │ • Graceful close      │ │ • Multi-peer conn map     │
          │ • 1 transport/stream  │ │ • Arc<Mutex<T>> per peer  │
          │                       │ │                           │
          │ Used by: tonic/gRPC   │ │ Used by: quinn/QUIC       │
          └───────────────────────┘ └───────────────────────────┘
                           │                 │
          ┌────────────────▼─────────────────▼──────────────────┐
          │          RdmaTransport (impl Transport)             │
          │  QP + CQ + MR + CM — Send/Recv two-sided            │
          │  (Future: RdmaRingTransport — Write + Ring)          │
          └─────────────────────────────────────────────────────┘
```

The Transport trait encapsulates all RDMA mechanics. Consumers only see:
- `send_copy(data)` → send bytes (transport picks internal buffer/slot)
- `poll_recv()` → `RecvCompletion { buf_idx, byte_len }`
- `recv_buf(buf_idx)` → `&[u8]` slice of received data
- `repost_recv(buf_idx)` → release buffer for reuse
- `poll_disconnect()` → disconnect detection + waker registration

### Core Types

```rust
/// RDMA-backed UDP socket for Quinn, generic over transport.
pub struct RdmaUdpSocket<T: Transport> {
    listener: AsyncCmListener,
    /// Shared connection map — same Arc given to senders so new
    /// connections are visible immediately.
    connections: Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<T>>>>>,
    local_addr: SocketAddr,
    /// In-progress accept future driven across poll_recv invocations.
    /// See rdma-transport-layer.md for the full poll_accept state machine.
    pending_accept: Option<Pin<Box<dyn Future<Output = Result<(SocketAddr, T)>> + Send>>>,
}

/// Per-task sender, shares the connection map via Arc<RwLock<>>.
pub struct RdmaUdpSender<T: Transport> {
    connections: Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<T>>>>>,
}

/// Convenience type alias for production use.
pub type DefaultRdmaUdpSocket = RdmaUdpSocket<RdmaTransport>;
```

### poll_recv Implementation

```rust
impl<T: Transport> AsyncUdpSocket for RdmaUdpSocket<T> {
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut count = 0;
        let mut dead_addrs = Vec::new();

        // 1. Drive in-progress accept state machine (non-blocking)
        self.poll_accept(cx)?;

        // 2. Poll all connections via Transport trait.
        // Each poll_recv(cx, ...) registers the waker with
        // that transport's CQ fd — so we're woken by ANY connection.
        for (addr, transport_arc) in &self.connections {
            let mut transport = transport_arc.lock().unwrap();

            if transport.is_qp_dead() {
                dead_addrs.push(*addr);
                continue;
            }

            while count < bufs.len() {
                let mut completions = [RecvCompletion::default(); 8];
                match transport.poll_recv(cx, &mut completions) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        for i in 0..n {
                            if count >= bufs.len() { break; }
                            let c = &completions[i];

                            // Datagrams: copy full packet, repost immediately
                            bufs[count][..c.byte_len].copy_from_slice(
                                &transport.recv_buf(c.buf_idx)[..c.byte_len]
                            );
                            meta[count] = RecvMeta {
                                addr: *addr,
                                len: c.byte_len,
                                stride: c.byte_len,
                                ecn: None,
                                dst_ip: Some(self.local_addr.ip()),
                            };
                            transport.repost_recv(c.buf_idx)?;
                            count += 1;
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        // Transport marks itself dead internally.
                        dead_addrs.push(*addr);
                        break;
                    }
                    _ => break,
                }
            }
        }

        // 3. Clean up dead connections
        for addr in dead_addrs {
            self.connections.remove(&addr);
        }

        if count > 0 { Poll::Ready(Ok(count)) } else { Poll::Pending }
    }

    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(RdmaUdpSender {
            connections: Arc::new(self.connections.clone()),
        })
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
```

### poll_send Implementation

```rust
impl<T: Transport> UdpSender for RdmaUdpSender<T> {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let connections = self.connections.read().unwrap();
        let transport_arc = connections.get(&transmit.destination)
            .ok_or_else(|| io::Error::new(
                io::ErrorKind::NotConnected, "no RDMA connection to peer"
            ))?;
        let mut transport = transport_arc.lock().unwrap();

        // Try to send FIRST — don't wait for a non-existent completion.
        let n = transport.send_copy(transmit.contents)
            .map_err(io::Error::other)?;
        if n > 0 {
            return Poll::Ready(Ok(()));
        }

        // All send buffers occupied — wait for one to complete, then retry.
        ready!(transport.poll_send_completion(cx)
            .map_err(|e| io::Error::other(e)))?;
        transport.send_copy(transmit.contents)
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(()))
    }
}
```

### Connection Management

QUIC is inherently connection-oriented (despite running over UDP). Each QUIC connection
to a unique remote address maps to one RDMA QP:

```
  Quinn Endpoint (single "socket")
       │
  RdmaUdpSocket<RdmaTransport>
    ├── Connection[peer_a:4433] → Arc<Mutex<RdmaTransport>> (64 recv bufs × 1.5KB)
    ├── Connection[peer_b:4433] → Arc<Mutex<RdmaTransport>> (64 recv bufs × 1.5KB)
    └── Listener → AsyncCmListener (accept new transports)
```

**Client path:** `connect()` triggers `Transmit` to a new address → `RdmaUdpSender` detects
no existing transport → initiates `RdmaTransport::connect()` → once connected, sends the
QUIC Initial packet via `transport.send_copy()`.

**Server path:** `RdmaUdpSocket::poll_recv` calls `poll_accept` → `RdmaTransport::accept()` →
new transport added to connection map → incoming QUIC Initial arrives via
`transport.poll_recv()`.

### Head-of-Line Blocking Analysis

Understanding HOL blocking across the different layers is critical for quinn-rdma design.

#### Layer 1: RC QP Transport — Hardware-Level HOL Blocking

RC (Reliable Connection) QP implements **TCP-like reliability in hardware**: ordered delivery,
acknowledgments, and automatic retransmission. This means it inherits TCP's HOL blocking problem.

If the sender posts packets A → B → C and packet B is lost on the wire:

```
Sender posts:    A → B → C
Wire:            A arrives, B lost, C arrives
HCA behavior:    Delivers A to recv buffer
                 Holds C in internal reorder buffer
                 Retransmits B (hardware retry, invisible to software)
                 Only after B is delivered does C's completion appear
Application:     Sees A .... [blocked waiting for B retry] .... B, C
```

This is the **exact same problem** that motivated QUIC being built on UDP instead of TCP.
TCP's in-order delivery blocks all streams when one packet is lost. QUIC on UDP can deliver
stream 2's data even when stream 1's packet is missing.

**The irony:** Running QUIC over RC RDMA reintroduces the HOL blocking that QUIC was designed
to eliminate. QUIC's stream multiplexing advantage is nullified because the RDMA layer enforces
ordering underneath.

**In practice:** RDMA networks (datacenter fabrics) have **extremely low loss rates** — often
<10⁻⁹. Hardware retry completes in microseconds when it does occur. The HOL blocking window is
essentially never triggered in the intended deployment environment (intra-datacenter). This is
exactly why both msquic and our design target datacenter use — RC's HOL blocking is acceptable
when packet loss is near-zero.

**msquic-rdma also uses RC** — the NDSPI `IND2Connector` + `IND2QueuePair` maps to RC semantics.
The connection-oriented handshake (Connect/Accept) and guaranteed delivery are inherent to the API.

#### Layer 2: Application Buffer — Our Design vs msquic

Even without wire-level losses, there is a separate HOL question at the application buffer level:
does one consumed-but-not-released packet block subsequent packet delivery?

**Our Design (Send/Recv): No application-level HOL blocking.**

Each of the pre-posted recv buffers is **independent**. When packet 3 completes in recv
buffer slot 3, it has zero effect on slots 0-2 and 4-7. There is no ordering dependency
between buffers. One slow-to-consume packet never blocks delivery of others.

```
Our recv buffers — no HOL:
  [buf0: ready] [buf1: empty] [buf2: ready] [buf3: empty] ...
  Each independent. buf2 delivery doesn't wait on buf0.
```

**msquic's Ring Buffer: Has application-level HOL blocking.**

The ring buffer `Head` pointer only advances when the consumer releases buffers **in order**.
If the QUIC core is slow to process the packet at `Head` (offset 0), the ring fills up
even though later offsets have been written and freed — their releases go into the
`CompletionTable` hash table and wait.

```
msquic ring buffer HOL:
  [Pkt_A][Pkt_B][Pkt_C][  free  ][  free  ]
   ^Head                          ^Tail
  If Pkt_A isn't consumed, Head can't advance.
  Tail eventually wraps → hits Head → ring full → sender stalls.
```

This is particularly problematic for a multiplexed protocol like QUIC, where packets carry
interleaved stream data. One slow stream's packet at the ring head blocks the entire ring
for all other streams.

#### Summary: HOL Blocking at Each Layer

| Layer | RC QP (ours + msquic) | UD QP (alternative) |
|-------|----------------------|---------------------|
| **Wire transport** | Yes — hardware retransmit blocks subsequent packets | No — packets can arrive out of order |
| **Application buffers (Send/Recv)** | No — independent recv buffers | No — independent recv buffers |
| **Application buffers (Write+Ring)** | Yes — ring head blocks release | N/A (UD doesn't support Write) |
| **Practical impact** | Negligible in datacenter (<10⁻⁹ loss) | Zero |

#### UD (Unreliable Datagram) as an Alternative

UD QP would actually be the **architecturally ideal** match for QUIC:

| Property | RC QP | UD QP | UDP (for reference) |
|----------|-------|-------|---------------------|
| **Reliability** | Hardware retransmit | None (like UDP) | None |
| **Ordering** | Strict FIFO | None (can reorder) | None |
| **HOL blocking** | Yes (wire-level) | No | No |
| **Connection** | 1 QP per peer | 1 QP for all peers | 1 socket for all peers |
| **RDMA Write/Read** | Supported | Not supported | N/A |
| **Max message size** | Up to 2GB | Path MTU only (~4KB) | 64KB |
| **Flow control** | RNR retry (hardware) | None (drop on full) | None (drop on full) |

**Why UD is attractive for QUIC:**
- No HOL blocking — QUIC handles reordering and loss recovery at the protocol layer
- No per-peer QP — single QP serves all connections (like UDP socket)
- QUIC's retransmission replaces hardware retransmission (no redundant reliability)
- Natural fit: UD is essentially "UDP over RDMA fabric"

**Why UD is not used in practice:**
1. **MTU limit (~4KB)** — UD packets cannot exceed path MTU; no hardware fragmentation.
   QUIC packets typically fit (≤1.2KB), but limits future jumbo frame optimization.
2. **No RDMA Write/Read** — only Send/Recv. No one-sided operations (not needed for QUIC anyway).
3. **Address Handle management** — need an `ibv_ah` per destination, managed manually.
4. **No flow control** — if receiver is slow, packets are silently dropped. QUIC handles
   this but drop rate could be high under load (unlike RNR which backpressures the sender).
5. **Less tested** — most RDMA deployments use RC; UD support in software providers (siw, rxe)
   may have edge cases.

**Recommendation:** Start with RC (proven, matches existing infrastructure, HOL is theoretical
in datacenter). Consider UD as a future optimization for workloads with many peers or where
QUIC's loss recovery is preferred over hardware retransmit.

#### Capacity Limits (Not HOL, But Important)

The real constraint in our design is **recv buffer count**, not ordering:

| Resource | Limit | Effect when exhausted |
|----------|-------|-----------------------|
| Pre-posted recv buffers | 32-64 per QP (proposed) | Sender gets RNR retry (hardware backoff) |
| Send Queue depth | `max_send_wr` per QP | `post_send` returns ENOMEM; wait for SendCQ |
| CQ depth | Fixed at creation | CQ overrun → QP ERROR state (fatal) |

With QUIC MTU (~1.2KB), 32 recv buffers can absorb 32 in-flight packets before RNR kicks in.
This is important because Quinn's `EndpointDriver` poll loop may not run between every packet
arrival — we need enough buffers to absorb bursts between polls.

**Recommendation:** Use 32-64 recv buffers per QP for quinn-rdma (vs 8 in AsyncRdmaStream),
since Quinn's event-driven architecture means less frequent polling than a dedicated stream reader.

### Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| **Abstraction** | `Transport` trait | Generic `<T>` from day one; mock for tests; ring transport future |
| **Initial transport** | `RdmaTransport` (Send/Recv) | 10× simpler than Write+ring; proven in AsyncRdmaStream |
| **CQ model** | Dual CQ (inside Transport) | Proven in our AsyncQp; avoids mixed-completion polling |
| **Recv buffers** | `TransportConfig::datagram()` (64 × 1.5KB) | Absorb bursts between Quinn poll cycles |
| **Connection setup** | Lazy (on first send) | Quinn sends packets to addresses; we connect on demand |
| **ECN** | Not supported initially | RDMA doesn't carry ECN; Quinn gracefully degrades |
| **GSO/GRO** | Not supported | Return `max_receive_segments() = 1`, `max_transmit_segments() = 1` |
| **MTUD** | Disabled | Return `may_fragment() = true` to disable path MTU discovery |
| **Concurrency** | `Arc<Mutex<T>>` per transport | Multiple UdpSenders share access; sync Mutex (brief hold, no await) |

## Comparison: msquic Approach vs Our Approach

| Aspect | msquic RDMA | quinn-rdma (proposed) |
|--------|------------|----------------------|
| **Integration point** | Internal C datapath plugin | External Rust trait implementation |
| **Code to write** | ~6000 lines | ~500-800 lines (estimate) |
| **RDMA verb** | Write + Immediate Data | Send / Recv |
| **QP type** | RC (reliable, ordered) | RC initially; UD possible future |
| **Ring buffer protocol** | Required (custom, ~500 lines) | Not needed |
| **Token exchange** | Required (post-connect handshake) | Not needed |
| **Connection state machine** | 13 states | 2 states (connecting, ready) |
| **Memory exposure** | Remote write to recv ring (MW protected) | None (two-sided) |
| **Wire-level HOL** | Yes (RC hardware retransmit) | Yes (RC); eliminable with UD |
| **App-level HOL** | Yes (ring buffer head blocks all) | No (independent recv buffers) |
| **Copies per packet** | 1 (sender only) | 2 (sender + receiver) |
| **Backpressure** | Ring buffer fullness + send queue | RNR retry (hardware) + CQ polling |
| **Recv notification** | Write-With-Immediate → RecvCQ | Recv completion → RecvCQ |
| **Completion model** | Windows IOCP (overlapped) | tokio AsyncFd (epoll) |
| **Modifies QUIC impl** | Yes (datapath callbacks, new APIs) | No (implements public trait) |
| **Portability** | MANA hardware only | All RDMA transports (IB, RoCE, iWARP, siw, rxe) |

## What We Gain from msquic Learnings

1. **Packet-oriented transport is natural for QUIC**: msquic proved that RDMA delivering
   discrete packets (not byte streams) is the right model for QUIC. Each QUIC packet = one
   RDMA operation. We don't need to build a byte stream (unlike our AsyncRdmaStream for gRPC).

2. **Dual CQ is validated**: msquic also uses separate send/recv CQs, confirming our design.

3. **We can skip the hard parts**: msquic's complexity comes from Write-based design (ring
   buffers, token exchange, offset sync, MW lifecycle). Using Send/Recv eliminates all of this.

4. **ECN is optional**: msquic doesn't implement ECN over RDMA either. Quinn handles missing
   ECN gracefully (just disables ECN-based congestion signals).

5. **Backpressure via CQ**: When the remote peer's recv buffers are full, RNR retry handles
   it at the hardware level. No application-level flow control needed (unlike msquic's ring
   buffer space tracking + send queue).

## Implementation Phases

### Phase 1: Transport Trait + RdmaTransport (prerequisite)
- Implement `Transport` trait in `rdma-io/src/transport.rs`
- Implement `RdmaTransport` in `rdma-io/src/rdma_transport.rs`
- Refactor `AsyncRdmaStream<T: Transport>` to use trait methods
- All existing tests must pass (see [rdma-transport-layer.md](rdma-transport-layer.md))

### Phase 2: Core RdmaUdpSocket (single connection)
- Implement `RdmaUdpSocket<T: Transport>` + `RdmaUdpSender<T>`
- Single-connection case: one `RdmaTransport` per socket
- Implement `AsyncUdpSocket` + `UdpSender` using `T: Transport` methods
- Test with Quinn client → Quinn server (server uses regular UDP)

### Phase 3: Multi-Connection + Server
- Add connection multiplexing (`HashMap<SocketAddr, Arc<Mutex<T>>>`)
- Add `AsyncCmListener` → `RdmaTransport::accept()` for server-side
- Lazy connection: `RdmaTransport::connect()` on first `poll_send` to unknown addr
- Test with both sides using RDMA

### Phase 4: Performance + Hardening
- Inline data for small packets (`TransportConfig.max_inline_data = 64`)
- Shared Receive Queue (SRQ) across QPs for memory efficiency
- Connection timeout / error handling / reconnection
- Benchmark against UDP baseline

## Open Questions

1. **Connection-on-first-send timing**: Quinn expects `poll_send` to be fast. RDMA CM connect
   takes milliseconds. Options: (a) return `WouldBlock` and connect in background, (b) require
   pre-connection via a custom API, (c) buffer the first packet and send after connect.

2. **Multiple QPs per endpoint**: Quinn uses one "socket" for all connections (like UDP).
   RDMA needs one QP per peer. The connection map adds overhead vs UDP's single socket.
   Alternative: shared receive queue (SRQ) to pool recv buffers across QPs.

3. **ECN signaling**: RDMA doesn't natively carry ECN. Could potentially encode ECN in
   immediate data or a header byte, but this adds complexity for minimal QUIC benefit
   (QUIC has its own congestion control).

4. **Path MTU**: RDMA connections have a negotiated MTU. Should we report this to Quinn
   via a custom mechanism, or let Quinn use its default?

## References

- [docs/design/rdma-transport-layer.md](rdma-transport-layer.md) — Transport trait + RdmaTransport design (authoritative)
- [quinn-rs/quinn](https://github.com/quinn-rs/quinn) — Rust QUIC implementation
- [quinn runtime traits](https://github.com/quinn-rs/quinn/blob/main/quinn/src/runtime/mod.rs) — AsyncUdpSocket, UdpSender
- [quinn-udp](https://github.com/quinn-rs/quinn/tree/main/quinn-udp) — Default UDP socket implementation
- [microsoft/msquic#5113](https://github.com/microsoft/msquic/pull/5113) — msquic RDMA transport (comparison)
- [docs/background/msquic-rdma.md](../background/msquic-rdma.md) — Our detailed analysis of msquic's approach
