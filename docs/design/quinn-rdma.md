# Quinn over RDMA — Design Document

**Status:** Implemented  
**Date:** 2026-03-14  
**Crate:** `rdma-io-quinn`  
**Source:** `rdma-io-quinn/src/lib.rs`  
**Tests:** `rdma-io-tests/tests/quinn_tests.rs`, `rdma-io-tests/tests/tonic_h3_tests.rs`  
**Dependencies:** `quinn 0.11`, `rdma-io`, `tonic-h3 0.0.5` (for gRPC/HTTP3 tests)

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
  RdmaUdpSocket<B: TransportBuilder> (impl AsyncUdpSocket)
    ├── try_send: drain send CQ (noop waker) + send_copy
    ├── poll_recv: poll_accept + iterate transports → fill bufs + meta
    │   └── Ok(0) credit-only batches: re-poll to register CQ waker
    ├── create_io_poller → RdmaUdpPoller (waits on blocked send CQ)
    ├── connections: Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<B::Transport>>>>>
    ├── accept_state: Mutex<Option<AcceptFuture>>
    └── listener: Arc<AsyncCmListener>
```

## Core Types

```rust
pub struct RdmaUdpSocket<B: TransportBuilder> {
    listener: Arc<AsyncCmListener>,
    connections: Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<B::Transport>>>>>,
    local_addr: SocketAddr,
    builder: B,
    accept_state: Mutex<Option<AcceptFuture<B::Transport>>>,
    send_blocked: Mutex<Option<Arc<Mutex<B::Transport>>>>,
    recv_waker: Mutex<Option<Waker>>,
    accept_errors: AtomicU32,
}

pub struct RdmaUdpPoller<B: TransportBuilder> {
    socket: Arc<RdmaUdpSocket<B>>,
}
```

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Crate | `rdma-io-quinn` (separate) | Quinn dependency isolated from core `rdma-io` |
| Generic | `RdmaUdpSocket<B: TransportBuilder>` | User chooses Send/Recv (`SendRecvConfig`) or Ring (`CreditRingConfig`) at construction |
| Connection map | `Arc<RwLock<HashMap>>` | `&self` API requires interior mutability; RwLock for read-heavy access |
| Accept | Builder's `accept()` in boxed future | TransportBuilder provides the accept logic; listener stashes interleaved ConnectRequests |
| try_send | Drain send CQ first (noop waker) | Prevents send buffer exhaustion; `send_copy` returns 0 → WouldBlock |
| UdpPoller | Waits on blocked transport's send CQ | Returns Pending until send buffer frees; avoids busy-spin |
| Pre-connect | `connect_to()` before Quinn endpoint | RDMA is point-to-point; must establish connection before data flow |
| Credit-only re-poll | `Ok(0)` → continue inner loop | Ensures CQ waker is registered (tokio edge-triggered); prevents missed notifications |
| ECN | Not supported | RDMA doesn't carry ECN; Quinn degrades gracefully |
| GSO/GRO | Not supported | `max_receive_segments() = 1`, `max_transmit_segments() = 1` |

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
  RdmaUdpSocket<B: TransportBuilder>
    ├── Peer[10.0.0.2:4433] → Arc<Mutex<B::Transport>> (per-builder config)
    ├── Peer[10.0.0.3:4433] → Arc<Mutex<B::Transport>> (per-builder config)
    └── Listener → AsyncCmListener (stashes interleaved ConnectRequests)
```

**Server:** `poll_recv` → `poll_get_request` → accept future → insert  
**Client:** `poll_send` to unknown addr → `SendRecvTransport::connect()` → lazy connect

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

1. ✅ `rdma-io-quinn` crate — `RdmaUdpSocket<B>`, `RdmaUdpPoller<B>`, `AsyncUdpSocket` impl, generic over `TransportBuilder`
2. ✅ `poll_accept` state machine — Uses builder's `accept()` in boxed future; `AsyncCmListener` stashes interleaved ConnectRequest events
3. ✅ Credit-only re-poll fix — `poll_recv` re-polls on `Ok(0)` to ensure CQ waker registration (edge-triggered safety)
4. ✅ Quinn echo + multi-peer tests — both Send/Recv and Ring transport variants
5. ✅ tonic-h3 gRPC tests — unary, server-streaming, multi-peer (both transports)

**Implementation findings:**
- Quinn 0.11.9 API differs from `main` branch (`try_send`/`UdpPoller` vs `UdpSender`)
- `try_send` must drain send CQ (noop waker) before attempting send — prevents buffer exhaustion
- RDMA requires `connect_to()` pre-connection — unlike UDP's implicit "send to anyone"
- Server endpoint must be created before client connects (poll_accept drives RDMA accept)

## tonic-h3 Integration

gRPC over HTTP/3 over QUIC over RDMA — the full stack works via `tonic-h3` (v0.0.5):

```
  tonic GreeterClient/Server
         │
  tonic-h3 H3Channel / H3Router  (gRPC ↔ HTTP/3)
         │
  h3 / h3-quinn                   (HTTP/3 ↔ QUIC)
         │
  quinn::Endpoint                  (QUIC state machine)
         │
  RdmaUdpSocket                    (AsyncUdpSocket → RDMA transport)
```

**Server:** `RdmaUdpSocket::bind(addr, builder)` → `Endpoint::new_with_abstract_socket(socket, server_config)` → `H3QuinnAcceptor` → `H3Router.serve()`

**Client:** `RdmaUdpSocket::bind(addr, builder)` → `connect_to(addr)` → `Endpoint::new_with_abstract_socket(socket, None)` → `H3QuinnConnector` → `H3Channel` → `GreeterClient`

**TLS config:** ALPN must be `"h3"` (not `"h2"`). Rustls `ServerConfig`/`ClientConfig` wrapped in `QuicServerConfig`/`QuicClientConfig`.

**Tested RPCs:** Unary `SayHello`, server-streaming `ServerStream` — both pass over RDMA.

## Known Issues & Improvements

### Should Fix

| # | Issue | Impact | Effort |
|---|-------|--------|--------|
| 1 | ~~**`poll_disconnect` never called in `poll_recv`**~~ | ✅ Fixed — replaced `is_qp_dead()` with `poll_disconnect(cx)` which registers CM fd waker for DREQ events and checks actual QP state. | N/A |
| 2 | ~~**`UdpPoller` always returns Ready → busy-spin**~~ | ✅ Fixed — `RdmaUdpPoller` now holds `Arc<RdmaUdpSocket>`; `try_send` stashes blocked transport on WouldBlock; `poll_writable` waits on its send CQ via `poll_send_completion(cx)`, returning `Pending` until a buffer frees. | N/A |
| 3 | ~~**Accept errors silently swallowed**~~ | ✅ Fixed — replaced `eprintln!` with `tracing::warn!`; added consecutive error counter (`AtomicU32`); propagates `io::Error` to Quinn after 10 consecutive failures. Counter resets on successful accept. | N/A |

### Nice-to-Have (future optimization)

| # | Feature | Benefit |
|---|---------|---------|
| 4 | **Lazy connect-on-first-send** — buffer first packet, spawn background CM connect | Removes `connect_to` requirement; true UDP-like behavior |
| 5 | **SRQ (Shared Receive Queue)** — pool recv buffers across QPs | 20 peers: 1280 → ~128 buffers (10× memory saving) |
| 6 | **Inline data for small packets** — `max_inline_data = 64` | QUIC packets ≤64B skip PCIe DMA; lower latency |
| 7 | **UD QP mode** — single QP for all peers | Eliminates per-peer QP overhead; true UDP semantics |
| 8 | **Ring buffer transport** — `CreditRingTransport` via `Transport` trait | One fewer copy on receiver; higher throughput |

## References

- [rdma-transport-layer.md](rdma-transport-layer.md) — Transport trait architecture
- [rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way comparison
- [rdma-credit-ring-transport.md](rdma-credit-ring-transport.md) — Future Ring transport
- [quinn-rs/quinn](https://github.com/quinn-rs/quinn) — Quinn source
- [msquic RDMA analysis](../background/msquic-rdma.md) — msquic research
