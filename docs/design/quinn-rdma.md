# Quinn over RDMA — Design Document

**Status:** Implemented  
**Date:** 2026-03-14  
**Crate:** `rdma-io-quinn`  
**Source:** `rdma-io-quinn/src/lib.rs`  
**Test:** `rdma-io-tests/tests/quinn_tests.rs`  
**Dependencies:** `quinn 0.11`, `rdma-io`

## Motivation

Quinn's `AsyncUdpSocket` trait provides a clean substitution point for RDMA transport
**without modifying Quinn itself**. Our `Transport` trait (implemented in Phase 1) handles
all RDMA mechanics — `rdma-io-quinn` just bridges Quinn's datagram API to our transport.

## Quinn 0.11 Socket Abstraction (actual API)

Quinn 0.11.9 uses a different API than `main` — `try_send` + `UdpPoller` instead of `UdpSender`:

```rust
// quinn 0.11.9 — the traits we implement
pub trait AsyncUdpSocket: Send + Sync + Debug + 'static {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>>;
    fn try_send(&self, transmit: &Transmit) -> io::Result<()>;  // sync, returns WouldBlock
    fn poll_recv(&self, cx, bufs, meta) -> Poll<io::Result<usize>>;  // &self, not &mut self
    fn local_addr(&self) -> io::Result<SocketAddr>;
    fn max_transmit_segments(&self) -> usize { 1 }
    fn max_receive_segments(&self) -> usize { 1 }
    fn may_fragment(&self) -> bool { true }
}

pub trait UdpPoller: Send + Sync + Debug + 'static {
    fn poll_writable(self: Pin<&mut Self>, cx) -> Poll<io::Result<()>>;
}
```

**Key difference from `main`:** All methods take `&self` (not `&mut self`), requiring
interior mutability (`Mutex`) for mutable transport state.

## Architecture

```
  quinn EndpointDriver
       │
  RdmaUdpSocket (impl AsyncUdpSocket)
    ├── try_send: drain send CQ (noop waker) + send_copy
    ├── poll_recv: poll_accept + iterate transports → fill bufs + meta
    ├── create_io_poller → RdmaUdpPoller (always writable)
    ├── connections: Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<RdmaTransport>>>>>
    ├── accept_state: Mutex<Option<AcceptFuture>>
    └── listener: Arc<AsyncCmListener>
```

## Core Types

```rust
pub struct RdmaUdpSocket {
    listener: Arc<AsyncCmListener>,
    connections: Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<RdmaTransport>>>>>,
    local_addr: SocketAddr,
    config: TransportConfig,
    accept_state: Mutex<Option<AcceptFuture>>,  // interior mutability for &self
}

pub struct RdmaUdpPoller;  // always reports writable

// No separate sender type — Quinn 0.11 uses try_send(&self) on the socket directly
```

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Crate | `rdma-io-quinn` (separate) | Quinn dependency isolated from core `rdma-io` |
| Concrete type | `RdmaTransport` (not generic `T`) | Quinn uses `Arc<dyn AsyncUdpSocket>` — concrete type simpler |
| Connection map | `Arc<RwLock<HashMap>>` | `&self` API requires interior mutability; RwLock for read-heavy access |
| Accept | `Mutex<Option<AcceptFuture>>` | Interior mutability; pinned future drives get_request + complete_accept |
| try_send | Drain send CQ first (noop waker) | Prevents send buffer exhaustion; `send_copy` returns 0 → WouldBlock |
| UdpPoller | Always returns Ready | RDMA send rarely blocks; hardware RNR handles backpressure |
| Pre-connect | `connect_to()` before Quinn endpoint | RDMA is point-to-point; must establish connection before data flow |
| ECN | Not supported | RDMA doesn't carry ECN; Quinn degrades gracefully |
| GSO/GRO | Not supported | `max_receive_segments() = 1`, `max_transmit_segments() = 1` |
| Config | `TransportConfig::datagram()` | 64 recv × 1.5KB, 4 send buffers per peer |

## poll_recv Flow

```
1. poll_accept(cx) — drive pending accept state machine
   - None: poll_get_request(cx) → start accept future
   - Some(fut): poll future → insert transport into connections

2. For each (addr, transport) in connections.read():
   - transport.poll_recv(cx, completions[..8])
     → Ready(Ok(n)): copy to bufs, fill meta, repost_recv
     → Ready(Err): collect dead addr
     → Pending: continue (waker registered)

3. Remove dead connections (write lock)
4. Return Ready(count) or Pending
```

## try_send Flow

```
1. connections.read() → get transport for transmit.destination
2. poll_send_completion with noop waker (drain completed sends)
3. transport.send_copy(transmit.contents)
   → Ok(n > 0): Ok(())
   → Ok(0): Err(WouldBlock) — Quinn calls poll_writable, then retries
   → Err: Err(e)
```

## Head-of-Line Blocking

| Layer | Impact | Notes |
|-------|--------|-------|
| Wire (RC hardware retransmit) | Negligible | Datacenter loss < 10⁻⁹ |
| App buffers (independent recv) | None | No ordering dependency between buffers |

## Connection Management

```
  RdmaUdpSocket<RdmaTransport>
    ├── Peer[10.0.0.2:4433] → Arc<Mutex<RdmaTransport>> (64 recv × 1.5KB)
    ├── Peer[10.0.0.3:4433] → Arc<Mutex<RdmaTransport>> (64 recv × 1.5KB)
    └── Listener → AsyncCmListener (poll_get_request)
```

**Server:** `poll_recv` → `poll_get_request` → accept future → insert  
**Client:** `poll_send` to unknown addr → `RdmaTransport::connect()` → lazy connect

## Comparison with msquic

| Aspect | msquic RDMA | rdma-io-quinn |
|--------|------------|---------------|
| Integration | Internal C plugin | External Rust trait |
| Code | ~6000 lines | ~300-500 lines |
| RDMA verb | Write + Immediate | Send / Recv |
| Ring protocol | Required | Not needed |
| Connection states | 13 | 2 |
| App-level HOL | Yes (ring head) | No (independent bufs) |
| Copies/packet | 1 | 2 |
| Modifies QUIC | Yes | No |
| Portability | MANA only | All RDMA providers |

## Implementation Status

All phases complete:

1. ✅ `rdma-io-quinn` crate — `RdmaUdpSocket`, `RdmaUdpPoller`, `AsyncUdpSocket` impl (~240 lines)
2. ✅ `poll_accept` state machine — `Arc<AsyncCmListener>` + pinned accept future
3. ✅ `complete_accept` on `RdmaTransport` — accepts pre-obtained `CmId` from `poll_get_request`
4. ✅ Quinn echo test — `rdma-io-tests/tests/quinn_tests.rs` (45 total tests pass)

**Implementation findings:**
- Quinn 0.11.9 API differs from `main` branch (`try_send`/`UdpPoller` vs `UdpSender`)
- `try_send` must drain send CQ (noop waker) before attempting send — prevents buffer exhaustion
- RDMA requires `connect_to()` pre-connection — unlike UDP's implicit "send to anyone"
- Server endpoint must be created before client connects (poll_accept drives RDMA accept)

## Open Questions

1. **Lazy connect timing:** CM connect takes ms; Quinn expects fast `poll_send`.
   Options: buffer first packet, or require pre-connection via custom API.

2. **SRQ optimization:** 20 peers × 64 recv bufs = 1280 buffers.
   Shared Receive Queue pools across QPs — future optimization.

## References

- [rdma-transport-layer.md](rdma-transport-layer.md) — Transport trait architecture
- [rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way comparison
- [rdma-ring-transport.md](rdma-ring-transport.md) — Future Ring transport
- [quinn-rs/quinn](https://github.com/quinn-rs/quinn) — Quinn source
- [msquic RDMA analysis](../background/msquic-rdma.md) — msquic research
