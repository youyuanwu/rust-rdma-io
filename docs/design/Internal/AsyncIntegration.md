# RDMA Async Integration — Design

> Internal design document for integrating `rdma-io` with Rust async runtimes.
>
> **Status:** Implemented (Phases A–F complete)  
> **Crate:** `rdma-io` (feature-gated: `async`, `tokio`)

---

## 1. Background

### The problem

Our synchronous `RdmaStream` uses spin-polling (`thread::yield_now()`) to wait for CQ completions. This works but wastes CPU and doesn't compose with async ecosystems. Users building high-performance servers need RDMA I/O to participate in their async event loop alongside TCP, timers, and other I/O sources.

### How RDMA completion notification works

RDMA provides an **event-driven** mechanism via `ibv_comp_channel`:

```
ibv_create_comp_channel()  →  returns ibv_comp_channel with .fd (a Linux file descriptor)
ibv_create_cq(..., channel)  →  associates CQ with the channel
ibv_req_notify_cq(cq)  →  arms the CQ to fire a notification on next completion
                           (must be called BEFORE posting WRs)
... work completions arrive ...
poll(channel.fd)  →  fd becomes readable when a CQ event fires
ibv_get_cq_event(channel)  →  consumes the notification (blocking if no event)
ibv_poll_cq(cq)  →  drains actual completions
ibv_ack_cq_events(cq, n)  →  acknowledges consumed events (required before CQ destroy)
ibv_req_notify_cq(cq)  →  re-arm for next notification
```

The key insight: **`comp_channel.fd` is a regular file descriptor** that can be registered with `epoll`/`kqueue`/`io_uring`. This means it can participate in any Rust async runtime's reactor.

### Why we switched to spin-polling (and why async fixes it)

The synchronous stream had a race condition with `ibv_comp_channel`:

1. `ibv_req_notify_cq()` — arm notification
2. `ibv_poll_cq()` — poll for completions (finds nothing)
3. ... completion arrives between 2 and 4 ...
4. `ibv_get_cq_event()` — **blocks forever** because the notification was consumed by the completion that arrived after we armed but before we blocked

The standard fix is the **drain-after-arm** pattern:

```
loop {
    ibv_req_notify_cq(cq)       // 1. arm first
    n = ibv_poll_cq(cq, ...)    // 2. poll — catches completions that arrived before arming
    if n > 0 { return }         // 3. got completions, no need to wait
    ibv_get_cq_event(channel)   // 4. no completions, safe to wait — any new completion
                                //    will trigger a notification since we armed in step 1
}
```

In async mode, step 4 becomes `async_fd.readable().await` — non-blocking, composable with the event loop.

---

## 2. Goals

1. **Async CQ polling** via `ibv_comp_channel` fd — no spin loops
2. **`AsyncRead` / `AsyncWrite`** on `RdmaStream` — using `futures::io` traits (runtime-agnostic), with tokio compat via `tokio_util::compat`
3. **Async RDMA verbs** — individual async wrappers for RDMA READ, WRITE, SEND, RECV, and atomic operations
4. **Runtime-agnostic core** with thin runtime adapters — support tokio and smol without code duplication
5. **Feature-gated** — `tokio` and `smol` support behind cargo features, core stays dependency-free
6. **Zero-cost when not using async** — sync API unaffected

### Non-goals

- Custom event loop / reactor implementation
- io_uring integration (separate future work)
- Async device events (`ibv_get_async_event`) — separate concern
- Direct `tokio::io::AsyncRead` impl — users use `tokio_util::compat::FuturesAsyncReadCompatExt` instead

---

## 3. Architecture

### Layered design

```
┌─────────────────────────────────────────────────┐
│  Quinn / tonic-h3 (external)                    │  ← QUIC/gRPC over RDMA
│  rdma-io-quinn (AsyncUdpSocket)                 │    (separate crate)
├─────────────────────────────────────────────────┤
│  AsyncRdmaStream / AsyncCmListener              │  ← High-level stream (§5)
│  (futures::io::AsyncRead + AsyncWrite)          │    SEND/RECV with buffering
├─────────────────────────────────────────────────┤
│  Transport trait / RdmaTransport                │  ← Datagram transport (§7.10)
│  send_copy() poll_recv() poll_disconnect()      │    Used by Quinn socket
├────────────────────────┬────────────────────────┤
│  AsyncQp               │                        │  ← Mid-level verb access (§7)
│  send() recv()         │  (AsyncRdmaStream uses │    All verbs: READ/WRITE/
│  read_remote()         │   AsyncQp internally)  │    SEND/RECV/Atomics
│  write_remote()        │                        │
│  compare_and_swap()    │                        │
├────────────────────────┴────────────────────────┤
│  AsyncCq (dual CQ: send_cq + recv_cq)          │  ← Async CQ poller (core)
│  (runtime-agnostic via CqNotifier trait)        │
├──────────────────┬──────────────────────────────┤
│  TokioCqNotifier │  (SmolCqNotifier — future)   │  ← Runtime adapters (feature-gated)
│  (AsyncFd)       │                              │
├──────────────────┴──────────────────────────────┤
│  AsyncCmId / AsyncCmListener / AsyncEventChannel│  ← Async CM (§8)
│  (async connect/accept/disconnect)              │
├─────────────────────────────────────────────────┤
│  CompletionChannel                              │  ← Safe wrapper around ibv_comp_channel
│  (owns fd, Drop calls ibv_destroy_comp_ch)      │
├─────────────────────────────────────────────────┤
│  QueuePair / CompletionQueue / MemoryRegion     │  ← Sync core (existing)
├─────────────────────────────────────────────────┤
│  rdma-io-sys FFI                                │  ← ibv_comp_channel, ibv_get_cq_event, etc.
└─────────────────────────────────────────────────┘
```

### Crate layout

```
rdma-io/
├── src/
│   ├── comp_channel.rs       # CompletionChannel (safe ibv_comp_channel wrapper)
│   ├── async_cq.rs           # AsyncCq + CqNotifier trait
│   ├── async_qp.rs           # AsyncQp — async verb wrappers (dual CQ)
│   ├── async_stream.rs       # AsyncRdmaStream (futures-io traits)
│   ├── async_cm.rs           # AsyncCmId / AsyncCmListener / AsyncEventChannel
│   ├── transport.rs          # Transport trait (generic datagram abstraction)
│   ├── rdma_transport.rs     # RdmaTransport (Send/Recv transport impl)
│   ├── tokio_notifier.rs     # TokioCqNotifier (behind "tokio" feature)
│   └── ... (sync modules: cm.rs, cq.rs, qp.rs, mr.rs, etc.)
└── Cargo.toml
```

### Feature flags

```toml
[features]
default = ["tokio"]

# Async runtime support
async = ["dep:futures-io"]           # Core async types (CqNotifier, AsyncCq, etc.)
tokio = ["async", "dep:tokio"]       # Tokio runtime adapter (AsyncFd)
# smol = ["async", "dep:async-io"]   # Future: smol runtime adapter

[dependencies]
tokio = { version = "1", features = ["net"], optional = true }
futures-io = { version = "0.3", optional = true }
```

---

## 4. Core Types

### 4.1 CompletionChannel

Safe wrapper around `ibv_comp_channel`. Provides the file descriptor used for async CQ notification. The fd is set non-blocking at creation and registered with the async reactor.

> **Implementation**: See `comp_channel.rs`. RAII drop calls `ibv_destroy_comp_channel()`. Implements `AsRawFd`.

### 4.2 CqNotifier trait (runtime abstraction)

Trait abstracting over async runtimes for CQ fd readiness. Each runtime provides a `CqNotifier` that registers the comp_channel fd with its reactor. Two methods:

- `readable()` — async future-based readiness (for `AsyncCq::poll()`)
- `poll_readable()` — poll-based readiness (for `AsyncRead`/`AsyncWrite` impls)

> **Implementation**: See `async_cq.rs` (trait definition), `tokio_notifier.rs` (tokio backend).

### 4.3 Tokio adapter

`TokioCqNotifier` wraps the comp_channel fd in `tokio::io::unix::AsyncFd` for reactor integration. Must be created within a tokio runtime context.

> **Implementation**: See `tokio_notifier.rs`.

### 4.4 Smol adapter

> 📋 **Future work**: `SmolCqNotifier` using `async_io::Async<FdWrapper>`. Same pattern as tokio adapter but using smol's reactor.

### 4.5 AsyncCq — the core async CQ poller

Async completion queue poller wrapping `CompletionQueue` + `CompletionChannel` + `CqNotifier`. Uses the drain-after-arm pattern (see §9 Performance). Provides:

- `poll()` — async: wait for completions, return when at least one available
- `poll_wr_id()` — async: wait for a specific WR ID completion
- `poll_completions()` — poll-based: for `AsyncRead`/`AsyncWrite` state machine via `CqPollState`
- Drop impl acks all remaining CQ events before destruction
- Batched acks every 16 events to reduce `ibv_ack_cq_events` mutex contention

> **Implementation**: See `async_cq.rs`.

---

## 5. AsyncRdmaStream

### Design

`AsyncRdmaStream` provides byte-stream I/O over RDMA using the `Transport` trait abstraction.

> **Implementation**: See `async_stream.rs`. Generic over `T: Transport` (defaults to `RdmaTransport`). Implements `futures::io::AsyncRead` + `AsyncWrite`.

Key design points:
- Generic over `Transport` trait — `RdmaTransport` default, mockable for testing
- **Dual CQ**: Separate `send_cq` and `recv_cq` avoid silent completion loss when both send and recv are in-flight concurrently. `poll_write` only waits on `send_cq`; `poll_read` only on `recv_cq`. No stashing needed.
- `recv_pending` tracks partial-read state (buf_idx, offset, total_len)
- `write_pending` tracks in-flight send WRs across poll calls
- `poll_close` / `shutdown()` — three-phase graceful disconnect (drain send → DREQ → await DISCONNECTED)

### Connection setup

Connection setup uses the CM event channel fd for true async I/O — no `spawn_blocking` needed.

**How it works**: The rdma_cm calls (`resolve_addr`, `resolve_route`, `connect`, `accept`) are non-blocking kernel submissions that return immediately. Results arrive as events on the `rdma_event_channel` fd, which is a regular Linux fd pollable with epoll/`AsyncFd`.

**Async connect flow**:
1. `EventChannel::new()` — set fd non-blocking via `fcntl(O_NONBLOCK)`
2. `cm_id.resolve_addr(addr)` — returns immediately (kernel async)
3. `await` fd readable → `get_event()` returns `ADDR_RESOLVED`
4. `cm_id.resolve_route()` — returns immediately
5. `await` fd readable → `get_event()` returns `ROUTE_RESOLVED`
6. Setup PD, CQ, QP, MRs, post recv buffers
7. `cm_id.connect()` — returns immediately
8. `await` fd readable → `get_event()` returns `ESTABLISHED`
9. Create `TokioCqNotifier` + `AsyncCq` (tokio reactor context available here)

**Async accept flow**:
1. Listener's `EventChannel` fd set non-blocking
2. `await` fd readable → `get_event()` returns `CONNECT_REQUEST`
3. Setup PD, CQ, QP, MRs on the new CM ID
4. `cm_id.accept()` — returns immediately
5. `await` fd readable → `get_event()` returns `ESTABLISHED`
6. `migrate()` to per-connection `EventChannel`

**Key constraint**: `TokioCqNotifier::new()` calls `AsyncFd::new()`, which requires tokio reactor context. This is fine — the notifier is created *after* connection setup completes, inside the `async fn` body.

**API surface**:
- `AsyncRdmaStream::connect(addr)` → `async fn` (was sync)
- `AsyncRdmaStream::connect_with_buf_size(addr, size)` → `async fn` (was sync)
- `AsyncRdmaListener::bind(addr)` → stays sync (no CM events needed for bind/listen)
- `AsyncRdmaListener::accept()` → `async fn` (was sync)

> See §8 for the `AsyncEventChannel` design that enables this.

### AsyncRead / AsyncWrite (futures::io — runtime-agnostic)

`futures::io::AsyncRead` + `AsyncWrite` are the **primary** trait implementations. Tokio users wrap with `.compat()`:

```rust
use tokio_util::compat::FuturesAsyncReadCompatExt;
let stream = AsyncRdmaStream::connect(&addr).await?;
let compat = stream.compat(); // now implements tokio::io::AsyncRead + AsyncWrite
tokio::io::copy(&mut compat, &mut sink).await?;
```

### poll_read / poll_write state machine

The challenge: `poll_read` must return `Poll::Pending` and register with the waker, but `AsyncCq::poll()` is async. Solution: `CqPollState` enum (Idle vs WaitingFd) in `AsyncCq::poll_completions()` drives the drain-after-arm loop in poll form, using `CqNotifier::poll_readable()` for fd readiness.

> **Implementation**: `CqPollState` enum and `poll_completions()` in `async_cq.rs`; `AsyncRead`/`AsyncWrite` impls in `async_stream.rs`.

---

## 6. Alternative: Simpler Approach via `poll_fn`

> **Historical note**: This was an alternative design considered before implementing Phase D. The actual implementation uses `CqPollState` + `poll_completions()` on `AsyncCq` instead of storing boxed futures. The `AsyncRdmaStream` also provides inherent async `read()`/`write()` methods (Phase C) alongside the trait impls.

---

## 7. Async RDMA Verbs (Individual Operations)

Beyond the stream abstraction (SEND/RECV), RDMA supports one-sided operations (READ/WRITE) and atomics that don't involve the remote CPU. These are the core value proposition of RDMA and should have first-class async support.

> **Implementation**: See `async_qp.rs` for `AsyncQp`, `mr.rs` for `RemoteMr`, `wr.rs` for `SendWr` builders.

### 7.1 Supported Verbs

| Verb | Direction | Remote CPU involved? | Use case |
|------|-----------|---------------------|----------|
| `SEND` / `RECV` | Two-sided | Yes (must post matching RECV) | Message passing, RPC |
| `RDMA_READ` | One-sided read | No | Remote memory access |
| `RDMA_WRITE` | One-sided write | No | Remote memory update |
| `RDMA_WRITE_WITH_IMM` | One-sided write | Yes (generates RECV completion) | Write + notification |
| `COMPARE_AND_SWAP` | Atomic | No | Distributed locking |
| `FETCH_AND_ADD` | Atomic | No | Distributed counters |

### 7.2 AsyncQp — Async Queue Pair

Central type for async verb operations. Owns a `CmQueuePair` (QP lifecycle) + `AsyncCq` (async completion). Field declaration order ensures correct teardown: QP drops before CQ. Provides async methods for each verb:

- **Two-sided**: `send()`, `recv()`, `send_with_imm()`
- **One-sided**: `read_remote()`, `write_remote()`, `write_remote_with_imm()`
- **Atomic**: `compare_and_swap()`, `fetch_and_add()`
- **Utility**: `post_send_signaled()` (post without awaiting, for poll-based callers)

Each method posts the WR via the underlying QP, then awaits completion via `AsyncCq::poll_wr_id()`.

### 7.3 RemoteMr

Lightweight descriptor exchanged between peers for one-sided operations:

```rust
pub struct RemoteMr {
    pub addr: u64,
    pub len: u32,
    pub rkey: u32,
}
```

Created via `OwnedMemoryRegion::to_remote()`. Exchanged out-of-band (e.g., via SEND/RECV during setup).

### 7.4 iWARP Limitations

- **RDMA WRITE with IMM**: Not defined in iWARP (RFC 5040) — InfiniBand/RoCE only. siw correctly rejects it.
- **Atomics**: siw reports `ATOMIC_NONE` — no CAS/FAA support. These are optional RDMA capabilities.
- **RDMA READ/WRITE**: Work fine on siw (iWARP requires them).
- API methods for all verbs exist and are correct for InfiniBand/RoCE; siw limitations are documented in tests.

### 7.6 Batch operations

> 📋 **Future work**: `post_send_batch()` — post multiple linked WRs and await all completions. Not yet implemented.

### 7.7 Relationship to AsyncRdmaStream

```
┌──────────────────────────────────────────────────┐
│  AsyncRdmaStream                                  │  High-level stream (§5)
│  (futures::io::AsyncRead + AsyncWrite)            │  Uses SEND/RECV internally
├──────────────────────────────────────────────────┤
│  AsyncQp                                          │  Mid-level verb access (§7)
│  send() recv() read_remote() write_remote()       │  All verbs, full control
│  compare_and_swap() fetch_and_add()               │
├──────────────────────────────────────────────────┤
│  AsyncCq                                          │  Low-level CQ polling (§4)
│  poll() poll_wr_id() poll_completions()           │  Runtime-agnostic
├──────────────────────────────────────────────────┤
│  CmQueuePair / CompletionQueue / MemoryRegion      │  Sync core (existing)
└──────────────────────────────────────────────────┘
```

- **`AsyncRdmaStream`** uses `AsyncCq` directly for SEND/RECV with buffering, flow control, and trait impls.
- **`AsyncQp`** gives direct access to all verbs — users manage MRs, WR IDs, and flow control themselves.
- Both share the **`AsyncCq`** layer for async completion handling.

### 7.8 Sync verb equivalents

> 📋 **Future work**: Ergonomic sync wrappers that post + spin-poll in one call (e.g., `QueuePair::read_remote_sync()`).

### 7.9 Prerequisites (all complete ✅)

All prerequisites for `AsyncQp` are implemented in the sync layer:
- `SendWr` builder methods: `set_remote()`, `set_imm_data()`, `atomic()`, `build_raw()` atomic branch
- `RemoteMr` type with `OwnedMemoryRegion::to_remote()`
- `WorkCompletion` accessors: `byte_len()`, `imm_data()`, `opcode()`
- `MemoryRegion::rkey()`, `MemoryRegion::addr()`, `AccessFlags` with remote access bits

---

## 8. Async CM Event Channel (Connection Setup) ✅

Fully implemented in `async_cm.rs`. True async connection setup using the CM event channel fd — no `spawn_blocking` needed.

### Components

- **`AsyncEventChannel`** — wraps `EventChannel` + tokio `AsyncFd<RawFd>`. Provides `get_event()` (async, loops until event) and `try_get_event()` (non-blocking). Edge-triggered safe: drains all events inside `readable()` guard.

- **`AsyncCmId`** — async client-side CM ID. Full connect pipeline: `resolve_addr()` → `resolve_route()` → `connect()`, each awaiting the corresponding CM event. Factory: `connect_to(addr)` for simple use.

- **`AsyncCmListener`** — async server-side listener. `bind()` / `bind_with_backlog()` (sync — no CM events for bind). `accept()` returns fully connected `AsyncCmId`. Two-phase variant: `get_request()` → `complete_accept()` for custom QP setup (used by Quinn socket).

- **`poll_get_request(cx)`** — poll-based accept for integration with Quinn's `poll_recv`. Used by `rdma-io-quinn::RdmaUdpSocket` to drive accept within the Quinn endpoint driver.

### Key design decisions

- **Per-connection EventChannel**: Every accepted connection is migrated via `rdma_migrate_id()` to its own `EventChannel`. This decouples connection fds so they can be independently polled, and prevents listener destruction from killing accepted connections.

- **Edge-triggered safety**: `AsyncEventChannel::get_event()` loops inside the `readable()` guard, consuming all available events before returning. This handles EPOLLET correctly (one edge per burst of events).

> **Implementation**: See `async_cm.rs` (~500 lines).

---

## 9. Performance Considerations

### Spin-poll vs. event-driven

| Approach | Latency | CPU | Use case |
|----------|---------|-----|----------|
| Spin-poll (`ibv_poll_cq` in loop) | ~1-3 μs | 100% one core | Ultra-low latency (HFT, MPI) |
| Event-driven (`comp_channel` + epoll) | ~5-15 μs | Near-zero idle | Servers, mixed workloads |
| Hybrid (poll briefly, then sleep) | ~2-5 μs | Adaptive | Best of both |

### Hybrid polling strategy

For latency-sensitive paths, we can offer a configurable hybrid:

```rust
pub struct AsyncCqConfig {
    /// Number of empty polls before sleeping on the fd.
    /// 0 = always event-driven, u32::MAX = always spin.
    pub spin_count: u32,
}

impl AsyncCq {
    pub async fn poll_hybrid(&self, wc: &mut [WorkCompletion], config: &AsyncCqConfig) -> Result<usize> {
        // 1. Spin-poll for spin_count iterations
        for _ in 0..config.spin_count {
            let n = self.cq.poll(wc)?;
            if n > 0 { return Ok(n); }
            std::hint::spin_loop();
        }
        // 2. Fall back to event-driven
        self.poll(wc).await
    }
}
```

### Batching CQ events

`ibv_ack_cq_events` takes a mutex internally — batch acknowledgements every 16 events (implemented in `AsyncCq`).

---

## 10. Testing Strategy

### Unit tests (no RDMA device)

- `CqNotifier` trait with a mock implementation
- Verify drain-after-arm loop logic
- Verify event acking and batching

### Integration tests (siw)

> **Implementation**: See `rdma-io-tests/tests/` — `async_cq_tests.rs` (3 tests), `async_qp_tests.rs` (8 tests), `async_stream_tests.rs` (12 tests), `quinn_tests.rs` (1 test), `tonic_h3_tests.rs` (3 tests), and more. Run with `RUST_TEST_THREADS=1`.

Tests cover: async CQ send/recv, AsyncQp ping-pong, AsyncRdmaStream echo/multi-message/large-transfer, futures-io traits, tokio compat, tokio::io::copy, RDMA WRITE+READ roundtrip, graceful shutdown (poll_close), Quinn echo over RDMA, tonic gRPC over HTTP/3 over QUIC over RDMA.

### Compatibility test matrix

| Test | tokio | smol | Notes |
|------|-------|------|-------|
| async_echo | ✅ | 📋 | Basic round-trip |
| async_multi_message | ✅ | 📋 | 5 round-trips |
| futures_io_echo | ✅ | 📋 | AsyncRead/AsyncWrite traits |
| tokio_compat_echo | ✅ | N/A | tokio_util compat layer |
| rdma_write_read | ✅ | 📋 | One-sided verbs |

---

## 11. Implementation Phases

### Phase A: CompletionChannel + AsyncCq ✅

1. Implement `CompletionChannel` (safe `ibv_comp_channel` wrapper)
2. Re-add `CompletionQueue::with_comp_channel` constructor
3. Implement `CqNotifier` trait
4. Implement `TokioCqNotifier` (tokio feature)
5. Implement `AsyncCq` with drain-after-arm loop
6. Test: create async CQ, post WR, await completion

### Phase B: AsyncQp — Async RDMA Verbs (SEND/RECV) ✅

Build the mid-level verb abstraction first so the stream can compose on top of it.

1. `AsyncQp` struct owning `CmQueuePair` + `AsyncCq` (safe constructor, field order = drop order)
2. Two-sided: `send()`, `recv()`, `send_with_imm()` (core verbs needed by stream)
3. Tests: async send/recv roundtrip, multi-message ping-pong

### Phase C: AsyncRdmaStream (wraps Transport) ✅

Stream is a thin buffering + flow-control layer on top of the `Transport` trait.

1. `AsyncRdmaStream::connect()` / `AsyncRdmaListener::bind()` / `accept()`
2. Generic over `T: Transport` — `RdmaTransport` default, enables mockable testing
3. `AsyncRdmaStream::read()` / `write()` delegate to `poll_read` / `poll_write` via `std::future::poll_fn` (single code path)
4. `accept()` uses `rdma_migrate_id()` to decouple accepted connection from listener event channel — allows listener to be safely dropped after accept
5. **Dual CQ**: Separate `send_cq` and `recv_cq` in `AsyncQp` — `poll_write` only waits on `send_cq`; `poll_read` only on `recv_cq`. No stashing or completion routing needed.
6. `poll_close` / `shutdown()` — three-phase graceful disconnect (drain send → DREQ → await DISCONNECTED)
7. Tests: async echo, multi-message, large transfer (32 KiB), graceful shutdown

### Phase D: AsyncRead / AsyncWrite trait impls ✅

1. Added `poll_readable()` to `CqNotifier` trait + `TokioCqNotifier` implementation
2. Added `CqPollState` enum and `poll_completions()` poll-based method to `AsyncCq`
3. Implemented `futures::io::AsyncRead` + `AsyncWrite` on `AsyncRdmaStream`
4. Tokio users use `tokio_util::compat::FuturesAsyncReadCompatExt` — no separate impl needed
5. Tests: futures-io echo, tokio compat echo, tokio::io::copy — 3 tests

### Phase E: AsyncQp — One-sided + Atomics ✅

Extend `AsyncQp` with the remaining RDMA verbs.

1. Sync prerequisites: `SendWr::atomic()` builder, `RemoteMr` type, `OwnedMemoryRegion::to_remote()` + `addr()`
2. One-sided: `read_remote()`, `write_remote()`, `write_remote_with_imm()`
3. Atomics: `compare_and_swap()`, `fetch_and_add()`
4. Tests: RDMA WRITE + READ roundtrip verified on siw — 1 test
5. Note: RDMA WRITE with IMM not supported on iWARP/siw; atomics require `ATOMIC_HCA` capability (siw has `ATOMIC_NONE`). These are tested on InfiniBand/RoCE hardware.

### Phase F: Async CM Event Channel ✅

True async connection setup — eliminated `spawn_blocking` from connect/accept paths.

1. **`EventChannel` extensions**: `fd()` accessor, `set_nonblocking()`, `try_get_event()` (non-blocking)
2. **`AsyncEventChannel`**: wraps `EventChannel` + `AsyncFd<RawFd>`, provides `async fn get_event()` + `expect_event()`
3. **`AsyncCmId`**: full async connect pipeline — `resolve_addr()` → `resolve_route()` → `connect()`, each awaiting CM events
4. **`AsyncCmListener`**: `accept()` (one-phase) + `get_request()` / `complete_accept()` (two-phase for custom QP setup) + `poll_get_request()` (poll-based for Quinn)
5. **`rdma_migrate_id`**: accepted connections migrated to per-connection `EventChannel`
6. Tests: all async stream + Quinn + tonic-h3 tests exercise this path

### Phase G: Transport Trait + RdmaTransport ✅

Generic datagram transport abstraction for Quinn QUIC integration.

1. **`Transport` trait** (`transport.rs`): `send_copy()`, `poll_recv()`, `poll_send_completion()`, `poll_disconnect()`, `is_qp_dead()`, `disconnect()`, `repost_recv()`
2. **`RdmaTransport`** (`rdma_transport.rs`): concrete impl using RDMA Send/Recv verbs with dual CQ, pre-posted recv buffers, and configurable buffer sizes
3. **`TransportConfig`**: presets `stream()` (NUM_RECV_BUFS=8, 32KB) and `datagram()` (64 recv × 1.5KB, 4 send)
4. Used by `rdma-io-quinn::RdmaUdpSocket` as the underlying transport for each peer connection
5. Used by `AsyncRdmaStream<T: Transport>` as default generic parameter

---

## 12. Dependencies

| Crate | Version | Feature | Purpose |
|-------|---------|---------|---------|
| `futures-io` | 0.3 | `async` | `AsyncRead`/`AsyncWrite` traits (primary, runtime-agnostic) |
| `tokio` | 1.x | `tokio` | `AsyncFd` for CQ + CM event channels |
| `tokio-util` | 0.7 | (test crate) | `FuturesAsyncReadCompatExt` for tokio compat |

All optional behind feature flags. Core `rdma-io` (sync) remains dependency-free (beyond FFI deps).

### Why `futures::io` over `tokio::io` as primary trait?

| Factor | `futures::io::AsyncRead` | `tokio::io::AsyncRead` |
|--------|--------------------------|------------------------|
| Runtime coupling | None — works everywhere | Requires tokio |
| Buffer type | `&mut [u8]` (standard) | `ReadBuf` (tokio-specific) |
| Compat direction | → tokio via `.compat()` (trivial) | → futures requires custom wrapper |
| smol/async-std | Native | Requires adapter |
| Trait crate size | `futures-io` (tiny, just traits) | `tokio` (large, pulls runtime) |

Tokio is the dominant runtime, but `futures::io` traits are the *de facto* standard for runtime-agnostic async I/O. The `tokio_util::compat` module provides zero-cost bridging — `FuturesAsyncReadCompatExt::compat()` wraps our type to implement `tokio::io::AsyncRead` with no overhead. Going the other direction (tokio→futures) has no standard adapter.

---

## 13. Open Questions

1. ~~**Should `AsyncCq` own the `CompletionChannel`?**~~ **Resolved** — Yes, `AsyncCq` owns the channel. Simpler lifetimes; 1:1 CQ-to-channel mapping.
2. ~~**Thread-safety of `AsyncCq`**~~ **Resolved** — `AsyncCq` is `Send` but not `Sync` (single-owner). `AsyncQp` is `Send + Sync` (QP operations are thread-safe per libibverbs).
3. ~~**`ibv_get_cq_event` blocking behavior**~~ **Resolved** — Returns `WouldBlock` on non-blocking fd. `poll_completions()` loops on spurious wakeups.
4. ~~**Multiple CQs on one channel**~~ **Resolved** — Not supported. 1:1 CQ-to-channel mapping. Dual CQ uses two separate channels.
5. ~~**Cancellation safety**~~ **Resolved** — Drain-after-arm is inherently cancellation-safe. Re-arm on every loop iteration, so dropping mid-poll is safe.
6. ~~**Shared CQ completion routing**~~ **Resolved** — Eliminated by dual CQ architecture. Send completions go to `send_cq`, recv to `recv_cq`. No routing needed.

---

## 14. Future Work

1. **Async timeouts** — Wrap verb/stream ops with `tokio::time::timeout()` or provide built-in `with_timeout(Duration)` combinators. Current design blocks forever on missing completions.
2. ~~**Graceful async shutdown**~~ **Done** — `poll_close` / `shutdown()` implemented with three-phase disconnect: drain pending sends → DREQ → await DISCONNECTED.
3. **Stream split** — `AsyncRdmaStream::into_split()` → `AsyncReadHalf` / `AsyncWriteHalf` for concurrent read+write from separate tasks. Requires shared transport (via `Arc`) and separate send/recv state.
4. **Error recovery & QP state transitions** — Document what happens when a verb fails mid-flight (QP transitions to error state). Distinguish fatal vs. recoverable errors. Design `reset_qp()` or reconnect flow.

### 14.1 Async ibverbs — why no new API is needed

The ibverbs posting functions (`ibv_post_send`, `ibv_post_recv`) are inherently non-blocking — they write a WR to the QP's send/recv queue and return immediately. The "async" part is waiting for the **completion**, which `AsyncQp` already handles:

```
AsyncQp::send()  =  ibv_post_send()  +  async_cq.poll_wr_id().await
AsyncQp::recv()  =  ibv_post_recv()  +  async_cq.poll_wr_id().await
```

There's no kernel or network wait in the posting step itself — the NIC processes WRs asynchronously by design. Making `ibv_post_send` "more async" would add overhead for no benefit. The completion-driven model (`AsyncCq`) is already the correct async abstraction.

**Raw ibv access**: Users who need `ibv_post_send`/`ibv_post_recv` without the completion await can use `AsyncQp::post_send_signaled()` (post-only) and `AsyncCq::poll()`/`poll_wr_id()` separately for manual control.

### 14.2 ~~Async disconnect~~ ✅ Implemented

Graceful async disconnect is implemented via `poll_close` / `shutdown()` on `AsyncRdmaStream`. Three-phase: drain pending sends → DREQ → await DISCONNECTED event. The `Transport` trait's `poll_disconnect(cx)` registers wakers on the CM fd for disconnect event detection.

### 14.3 Async CM event stream

For advanced use cases (connection managers, multi-connection servers), a `Stream`-based API could be useful:

```rust
impl AsyncCmListener {
    pub fn events(&self) -> impl Stream<Item = Result<CmEvent>> + '_ { ... }
}
```

**Dependencies**: Requires `futures::Stream`. Implementation wraps `AsyncEventChannel::get_event()` in a `poll_fn`-based stream adapter.

### 14.4 SmolCqNotifier (smol runtime support)

`SmolCqNotifier` using `async_io::Async<FdWrapper>` — same pattern as tokio adapter but using smol's reactor. Behind `smol` feature flag.

### 14.5 Additional future items

- **Cancellation safety documentation** — Document precisely which async operations are safe to cancel (drop the future) and which may leak posted WRs.
- **Backpressure / flow control (async)** — Track remote RECV buffer availability; `write()` should return `Pending` when no credits available.
- **SRQ async support** — `AsyncSrq` for connection-dense servers where thousands of QPs share a receive pool.
- **io_uring integration** — Use `io_uring` for CQ notification, eliminating syscall overhead.
