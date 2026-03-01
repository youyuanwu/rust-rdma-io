# RDMA Safe Rust API — Design

> Design for `rdma-io`, the safe Rust wrapper over `rdma-io-sys`.

---

## 1. Goals

1. **Memory-safe RAII wrappers** for all RDMA resources (context, PD, CQ, QP, MR, SRQ, AH, etc.)
2. **Both ibverbs APIs**: legacy (`ibv_post_send`/`ibv_poll_cq`) and new (`ibv_wr_*`/`ibv_start_poll`)
3. **rdma_cm** connection management with a listen/connect model
4. **`std::io::Read`/`Write`** trait implementations for RDMA streams
5. **Zero-cost where possible** — thin wrappers that compile down to the same code as C

### Non-goals (for now)

- Provider-specific APIs (mlx5dv)
- Async runtime integration (tokio, smol) — deferred to future work
- io_uring integration (future candidate)
- Connection pooling / reconnection (application-level concern)

---

## 2. Crate Structure

```
rust-rdma-io/
├── rdma-io-sys/         # FFI bindings (exists today)
├── rdma-io/             # Safe wrapper crate (this design)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── device.rs    # Device enumeration and context
│   │   ├── pd.rs        # Protection Domain
│   │   ├── cq.rs        # Completion Queue (legacy + extended)
│   │   ├── qp.rs        # Queue Pair (legacy + extended)
│   │   ├── mr.rs        # Memory Region
│   │   ├── srq.rs       # Shared Receive Queue
│   │   ├── ah.rs        # Address Handle
│   │   ├── comp.rs      # Completion Channel
│   │   ├── cm.rs        # rdma_cm connection management
│   │   ├── wr.rs        # Work requests (send/recv builders)
│   │   ├── wc.rs        # Work completions
│   │   ├── error.rs     # Error types
│   │   └── stream.rs    # Read/Write over RDMA
│   └── Cargo.toml
└── rdma-io-tests/       # Integration tests (exists today, will grow)
```

---

## 3. Resource Ownership Model

RDMA resources form a **directed acyclic graph** of ownership. Destroying a parent before its children is undefined behavior in libibverbs. The safe API must enforce this statically or dynamically.

### Resource hierarchy

```
Device
 └── Context
      ├── ProtectionDomain (PD)
      │    ├── MemoryRegion (MR)
      │    ├── QueuePair (QP)       ← also references CQ, optionally SRQ
      │    ├── AddressHandle (AH)
      │    └── MemoryWindow (MW)
      ├── CompletionQueue (CQ)
      │    └── (referenced by QP)
      ├── SharedReceiveQueue (SRQ)
      │    └── (referenced by QP)
      └── CompletionChannel
           └── (referenced by CQ)
```

### Approach: `Arc`-based parent references

Each resource holds an `Arc` to its parent, ensuring the parent outlives all children. This is the same approach used by Nugine/rdma and is the simplest correct solution.

Every resource type implements `Drop`, calling the corresponding `ibv_destroy_*` / `ibv_dealloc_*` / `ibv_dereg_*` function. Drop order is guaranteed correct by the `Arc` references — children are dropped before parents because the parent's refcount only reaches zero after all children are gone.

> **Implementation**: See `device.rs`, `pd.rs`, `cq.rs`, `qp.rs`, `mr.rs` for the actual struct definitions.

### Why `Arc` and not lifetimes?

Pure lifetimes would be more zero-cost, but RDMA resource graphs are typically shared across threads (e.g., one thread posts sends, another polls CQ). `Arc` enables `Send + Sync` naturally. The overhead is one atomic increment per resource creation — negligible compared to kernel/driver round-trips.

### MemoryRegion is special

An MR pins a memory buffer for DMA access. The buffer **must not be freed or moved** while the MR is registered. Two approaches:

1. **Borrowed MR** (`MemoryRegion<'a>`) — ties MR lifetime to buffer lifetime via `PhantomData<&'a mut [u8]>`. Simple, but buffer must outlive all QP operations referencing it.
2. **Owned MR** (`OwnedMemoryRegion`) — MR owns its buffer (e.g., `Vec<u8>`). The buffer is freed when the MR is deregistered. Simpler lifetime management for long-lived buffers.

We provide both:

- **Borrowed MR** (`MemoryRegion<'a>`) — ties MR lifetime to buffer via `PhantomData<&'a mut [u8]>`
- **Owned MR** (`OwnedMemoryRegion`) — owns its buffer (`Box<[u8]>`), freed on deregistration

> **Implementation**: See `mr.rs`. Also includes `RemoteMr` descriptor for one-sided operations and `OwnedMemoryRegion::to_remote()` for remote MR exchange.

---

## 4. Core API Surface

> **Implementation**: All types below are implemented. See source files for full API signatures.

### 4.1 Device & Context (`device.rs`)

- `devices()` — list RDMA devices
- `Device::open()` → `Arc<Context>`
- `Context`: `query_device()`, `query_port()`, `query_gid()`, `create_pd()`, `create_cq()`

### 4.2 Protection Domain (`pd.rs`)

- `reg_mr()` — borrowed MR with lifetime tied to buffer
- `reg_mr_owned()` — owned MR with internal buffer
- `create_qp()` — QP creation with explicit CQ references

### 4.3 Completion Queue (`cq.rs`)

- `poll()` — poll for completions (legacy API)
- `req_notify()` — arm notification for async use
- `CompletionQueueEx` / `PollGuard` — new CQ API (designed, not yet implemented — siw lacks support)

### 4.4 Queue Pair (`qp.rs`)

- `post_send()` / `post_recv()` — legacy verb posting
- `modify()` — QP state transitions (INIT → RTR → RTS)
- `QueuePairEx` / `WrSession` — new WR builder API (designed, not yet implemented)

### 4.5 Memory Region (`mr.rs`)

- `MemoryRegion<'a>` — borrowed, `OwnedMemoryRegion` — owned
- Both expose `lkey()`, `rkey()`, `as_slice()`, `as_mut_slice()`
- `RemoteMr` — lightweight descriptor (addr, rkey, len) for one-sided ops

### 4.6 Work Completion (`wc.rs`)

- `#[repr(transparent)]` wrapper around `ibv_wc`
- Typed accessors: `status()`, `opcode()`, `byte_len()`, `wr_id()`, `imm_data()`

---

## 5. RDMA CM (Connection Manager)

rdma_cm provides TCP-like connection semantics over RDMA. This is the primary way to use iWARP (siw) and the recommended approach for RoCE.

> **Implementation**: See `cm.rs` for `EventChannel`, `CmId`, `CmEvent`, `ConnParam`, `PortSpace`.

### Key types

- **`EventChannel`** — receives CM events (`get_event()`)
- **`CmId`** — the core CM handle: `resolve_addr()`, `resolve_route()`, `connect()`, `listen()`, `accept()`, `disconnect()`, `create_qp()`, `create_qp_with_cq()`, `migrate()`
- **`CmEvent`** — typed event with `event_type()`, `cm_id_raw()`, `ack()`

### Connection flows

**Client**: `CmId::new()` → `resolve_addr()` → `resolve_route()` → `create_qp()` → `connect()` → `ESTABLISHED`

**Server**: `CmId::new()` → `listen()` → `CONNECT_REQUEST` → `from_raw()` → `create_qp()` → `accept()` → `ESTABLISHED`

> **Note**: `accept()` in the async path uses `rdma_migrate_id()` to give accepted connections their own event channel, decoupling from listener lifetime.

---

## 6. Async Completion Polling

> **Implementation**: See `async_cq.rs`, `tokio_notifier.rs`, `async_qp.rs`. Full async design in [AsyncIntegration.md](AsyncIntegration.md).

### Design

RDMA completion notifications use file descriptors (via `ibv_comp_channel`). We register the fd with an event loop for edge-triggered readiness, then poll the CQ when notified.

- **`CqNotifier` trait** — abstracts runtime-specific fd polling (`wait_readable()` + `poll_readable()`)
- **`AsyncCq`** — drain-after-arm loop with batched acks (every 16 events), plus `poll_completions()` for poll-based `AsyncRead`/`AsyncWrite`
- **`AsyncQp`** — async verb wrappers: `send()`, `recv()`, `send_with_imm()`, plus one-sided: `read_remote()`, `write_remote()`, `compare_and_swap()`, `fetch_and_add()`
- **`TokioCqNotifier`** — tokio backend using `AsyncFd`

### Completion notification flow (drain-after-arm)

1. `ibv_req_notify_cq()` to arm
2. `ibv_poll_cq()` to drain (catches completions between arm and await)
3. If nothing found: await fd readiness via `CqNotifier`
4. `ibv_get_cq_event()` to consume the notification
5. Batch ack every 16 events (`ibv_ack_cq_events`)
6. Loop back to step 1

This drain-after-arm pattern avoids the race condition where a completion arrives between arming and blocking.

---

## 7. Stream Abstraction (Read / Write)

> **Implementation**: `stream.rs` (sync), `async_stream.rs` (async). Async traits in [AsyncIntegration.md](AsyncIntegration.md).

- **`RdmaStream`** — sync stream with `std::io::Read`/`Write` over RDMA SEND/RECV via rdma_cm
- **`RdmaListener`** — `bind()` + `accept()`, like `TcpListener`
- **`AsyncRdmaStream`** — async equivalent, implements `futures::io::AsyncRead`/`AsyncWrite`
- **`AsyncRdmaListener`** — async listener; `accept()` uses `rdma_migrate_id()` for listener lifetime independence
- Tokio users use `tokio_util::compat::FuturesAsyncReadCompatExt` for `tokio::io` traits

### How it works

- **Write path**: Copy user data into pre-registered send buffer → post SEND WR → await completion
- **Read path**: Pre-post RECV WRs with registered buffers → await completion → copy to user buffer
- **Buffer management**: Double-buffering (one active, one being filled) to overlap I/O
- **Flow control**: Credit-based — sender tracks how many RECV buffers the remote has posted

This is an opinionated, high-level abstraction. Users who need fine-grained control use the lower-level QP/CQ APIs directly.

---

## 8. Error Handling

> **Implementation**: See `error.rs`.

- `Error` enum with variants: `Verbs`, `Cm`, `WorkCompletion`, `NoDevices`, `DeviceNotFound`, `InvalidArg`
- `from_ret()` for ibverbs (negative errno), `from_ret_errno()` for rdma_cm (-1 + errno)
- Uses `thiserror` for `Display`/`Error` derives, `std::io::Error` as inner source

---

## 9. Thread Safety

| Type | `Send` | `Sync` | Notes |
|------|--------|--------|-------|
| `Context` | ✅ | ✅ | ibverbs contexts are thread-safe |
| `ProtectionDomain` | ✅ | ✅ | Thread-safe |
| `CompletionQueue` | ✅ | ✅ | `ibv_poll_cq` is thread-safe (serialized internally) |
| `QueuePair` | ✅ | ✅ | `ibv_post_send`/`ibv_post_recv` are thread-safe |
| `MemoryRegion` | ✅ | ✅ | Read-only handle; buffer access is user's responsibility |
| `OwnedMemoryRegion` | ✅ | ❌ | Owns mutable buffer — `Send` but not `Sync` |
| `CmId` | ✅ | ❌ | CM operations are not thread-safe |
| `EventChannel` | ✅ | ❌ | Single reader at a time |
| `WrSession` | ❌ | ❌ | Scoped to one thread, one QP |
| `PollGuard` | ❌ | ❌ | Scoped to one thread, one CQ |

---

## 10. Feature Flags

```toml
[features]
default = ["legacy-api"]

# Legacy ibverbs API (ibv_post_send, ibv_poll_cq)
legacy-api = []

# New ibverbs API (ibv_wr_*, ibv_start_poll)
new-api = []

# rdma_cm connection management
cm = []

# Read/Write stream abstraction over rdma_cm
stream = ["cm"]

# Async runtime: tokio backend (AsyncFd-based CqNotifier)
tokio = ["dep:tokio"]

# Async runtime: smol backend (async-io-based CqNotifier)
smol = ["dep:async-io"]
```

> **Note**: `tokio` and `smol` feature flags are implemented. The `legacy-api`, `new-api`, `cm`, and `stream` flags are designed but not yet enforced — everything compiles unconditionally.

---

## 11. Enum / Flag Types

> **Implementation**: See `enums.rs`, `flags.rs`, and individual module files.

The FFI layer generates enums as `u32` type aliases with module-level constants. The safe API wraps these in proper Rust types:

- **Rust enums** for transport types (`QpType`), states (`QpState`, `WcStatus`, `WcOpcode`), event types (`CmEventType`)
- **`bitflags!`** for flag fields (`AccessFlags`, `QpAttrMask`, `SendFlags`)

---

## 12. Implementation Phases

### Phase 1: Core resources + legacy API ✅

- `device.rs`, `pd.rs`, `cq.rs`, `qp.rs`, `mr.rs`, `error.rs`, `wc.rs`, `wr.rs`
- RAII wrappers with `Arc`-based ownership
- Legacy `post_send`/`post_recv`/`poll_cq`
- QP state transitions (INIT → RTR → RTS)
- Enum and flag types via `bitflags`
- Tests against siw (extend existing test suite)
- 10 safe API tests passing

### Phase 2: rdma_cm ✅

- `cm.rs` — `EventChannel`, `CmId`, `CmEvent`, `ConnParam`, `PortSpace`
- Connection setup (client + server flows)
- `from_ret_errno()` for rdma_cm error handling (returns -1 + errno, unlike ibverbs)
- `CmId::create_qp_with_cq()` for explicit CQ passing
- Tests: siw loopback connect/disconnect, data transfer via CM (2 tests)

### Phase 3: New ibverbs API ⏭️ (skipped)

- `QueuePairEx`, `WrSession` (builder pattern for new WR API)
- `CompletionQueueEx`, `PollGuard` (new CQ polling API)
- **Skipped**: siw does not support `ibv_create_cq_ex` or `ibv_qp_to_qp_ex` (returns `EOPNOTSUPP`). Will implement when hardware or rxe testing is available.

### Phase 4: Stream ✅

- `stream.rs` — `RdmaStream`, `RdmaListener`
- `std::io::Read`/`std::io::Write` implementations
- Spin-polling CQ design (comp_channel approach had race conditions with siw)
- Double-buffered recv with partial-read support
- Tests: echo, multi-message (5 round-trips), large transfer (32 KiB) — 3 tests

### Async Phase A: CompletionChannel + AsyncCq ✅

- `comp_channel.rs` — Safe `ibv_comp_channel` wrapper with non-blocking fd
- `async_cq.rs` — `CqNotifier` trait + `AsyncCq` with drain-after-arm loop, batched acks
- `tokio_notifier.rs` — `TokioCqNotifier` using `tokio::io::unix::AsyncFd`
- Feature flags: `tokio`, `smol` (conditional compilation via `#[cfg(feature = "...")]`)
- Dependencies: `futures-io`, `tokio` (optional), `async-io` (optional), `libc`
- Tests: async CQ send/recv, poll_wr_id — 2 tests

### Async Phase B: AsyncQp ✅

- `async_qp.rs` — `AsyncQp` wrapping raw `*mut ibv_qp` + `AsyncCq`
- Async verbs: `send()`, `recv()`, `send_with_imm()`, `poll()`
- Borrows raw QP pointer (CM ID owns the QP, avoids double-free)
- Tests: send/recv roundtrip, multi-message ping-pong — 2 tests

### Async Phase C: AsyncRdmaStream ✅

- `async_stream.rs` — `AsyncRdmaStream` + `AsyncRdmaListener`
- TCP-like async read/write over RDMA SEND/RECV via shared `AsyncCq`
- `read()`/`write()` delegate to `poll_read`/`poll_write` via `std::future::poll_fn` (single code path)
- `accept()` uses `rdma_migrate_id()` to give accepted connections their own event channel, decoupling from listener lifetime
- Shared CQ completion stashing via `VecDeque` — recv and send completions never dropped across read/write interleaving
- Connection setup is synchronous (use `spawn_blocking`); data path is fully async
- Tests: echo, multi-message (5 round-trips), large transfer (32 KiB) — 3 tests

### Async Phase D: AsyncRead / AsyncWrite traits ✅

- `poll_readable()` added to `CqNotifier` trait for poll-based fd readiness
- `CqPollState` enum + `poll_completions()` on `AsyncCq` for poll-based drain-after-arm
- `futures::io::AsyncRead` + `AsyncWrite` implemented on `AsyncRdmaStream`
- Tokio users use `tokio_util::compat::FuturesAsyncReadCompatExt` for `tokio::io` traits
- Tests: futures-io echo, tokio compat echo, tokio::io::copy — 3 tests

### Async Phase E: One-sided + Atomics ✅

- `RemoteMr` type + `OwnedMemoryRegion::to_remote()` for remote MR descriptors
- `SendWr::atomic()` builder for CAS/FAA work requests
- `AsyncQp`: `read_remote()`, `write_remote()`, `write_remote_with_imm()`, `compare_and_swap()`, `fetch_and_add()`
- Tests: RDMA WRITE + READ roundtrip on siw — 1 test
- Note: RDMA WRITE with IMM (iWARP limitation) and atomics (`ATOMIC_NONE` on siw) require InfiniBand/RoCE hardware
- See [AsyncIntegration.md](AsyncIntegration.md) for remaining phase (F)

### Phase 5: Advanced resources 📋 (future)

- `srq.rs` — Shared Receive Queue
- `ah.rs` — Address Handle (for UD)
- UD QP support
- Memory Window (MW), Device Memory (DM)

---

## 13. Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Ownership model | `Arc`-based | Enables `Send + Sync`, natural for multi-threaded RDMA |
| MR lifetime | Borrowed + Owned variants | Flexibility — short-lived vs long-lived buffers |
| Async approach | `futures::io` traits as primary, runtime-agnostic | Tokio users use `tokio_util::compat`; smol works natively. See [AsyncIntegration.md](AsyncIntegration.md) |
| Async CQ pattern | Drain-after-arm loop | Avoids race between `ibv_req_notify_cq` and `ibv_get_cq_event` that caused hangs with standard arm-then-block |
| Async CQ ack batching | Batch every 16 events | `ibv_ack_cq_events` takes a mutex; batching reduces contention |
| AsyncQp ownership | Borrows raw `*mut ibv_qp` | CM ID owns the QP; `QueuePair::Drop` would double-free. `AsyncQp` uses unsafe raw pointer with documented lifetime requirement |
| Accepted CM ID migration | `rdma_migrate_id()` after accept | Accepted connections inherit listener's event channel; dropping listener kills accepted QPs. Migration decouples lifetimes |
| Feature flags (async) | `tokio`, `smol` | Core async types (`CqNotifier`, `AsyncCq`, `AsyncQp`) compile with either; notifier backends are feature-gated |
| Error handling | `thiserror` + `std::io::Error` | Composable with Rust I/O ecosystem |
| Drop error handling | `tracing::error!` on all resource destruction failures | Silent drops hide kernel resource leaks |
| Enum wrapping | `bitflags` for flags, Rust enums for types | Type safety without runtime cost |
| API surface | Both legacy and new ibverbs | Legacy for compatibility, new for performance |
| Connection mgmt | rdma_cm as primary | Required for iWARP, recommended for RoCE |
| Stream abstraction | `std::io::Read`/`Write` (sync), `futures::io::AsyncRead`/`AsyncWrite` (async) | Sync: `std::io`; Async: poll-based via `CqPollState`. Tokio: `tokio_util::compat` |
| Stream CQ polling | Spin-poll + `thread::yield_now()` | comp_channel had race condition (notification consumed before `ibv_get_cq_event`) |
| rdma_cm error model | `from_ret_errno()` (reads `last_os_error`) | rdma_cm returns -1 + sets errno, unlike ibverbs which returns negative errno |
| Test serialization | `RUST_TEST_THREADS=1` in `.cargo/config.toml` | siw has kernel resource contention with concurrent RDMA connections |

---

## 14. Open Questions

1. **Buffer pool**: Should we provide a built-in registered buffer allocator, or leave it to users?
2. **Inline data threshold**: Should `post_send` auto-detect inline capability and use it when data is small?
3. **SRQ integration**: How tightly should SRQ be coupled with QP creation?
4. **Multi-device**: Should `RdmaStream` support failover between devices?
5. ~~**Metrics/tracing**: Should we instrument with `tracing` crate from the start?~~ **Resolved** — `tracing` added for Drop error reporting; data-path tracing deferred to future work (§15.5).

---

## 15. Future Work

Items identified from the [ExistingLibs.md](ExistingLibs.md) gap analysis and implementation review:

### API Gaps (design specified but not yet implemented)

11. **`MemoryRegion<'a>` slice accessors** — Add `as_slice()` / `as_mut_slice()` on borrowed MRs (already on `OwnedMemoryRegion`).
12. ~~**`CompletionChannel`**~~ — **Done**: `comp_channel.rs` wraps `ibv_comp_channel` with non-blocking fd, `AsRawFd`, RAII drop.
13. **`qp_type()` accessor** — Return the transport type of a `QueuePair`.
14. **`CmId.qp()` / `CmId.pd()` safe accessors** — Return `&QueuePair` / `&ProtectionDomain` instead of raw pointers.
15. **`query_gid_table()`** — Bulk GID table query for device discovery.
16. **Feature flags** — §10 design specifies `legacy-api`, `new-api`, `cm`, `stream` features but none are enforced; everything compiles unconditionally. `tokio` and `smol` flags are implemented and working.

### Stream Enhancements

17. **Read/write timeouts** — `set_read_timeout()` / `set_write_timeout()` like `TcpStream`. Current spin-poll blocks forever.
18. **`RdmaListener::local_addr()`** — Analogous to `TcpListener::local_addr()`, useful for ephemeral port tests.
19. **`RdmaStream::peer_addr()`** — Connection introspection.
20. **Stream split** — `into_split()` returning `OwnedReadHalf` / `OwnedWriteHalf` (like tokio's `TcpStream`), or `Clone` via internal `Arc`.
21. **Builder pattern** — `RdmaStreamBuilder::new(addr).buf_size(128*1024).max_recv_wr(32).connect()?` for ergonomic configuration.
22. **Graceful shutdown protocol** — `shutdown(Write)` → drain reads → disconnect, instead of abrupt disconnect in Drop.
23. **Backpressure / flow control** — Credit-based flow control (§7 design mentions it but not implemented). Sender must track remote recv buffer availability to avoid deadlock when sender outpaces receiver.

### Ergonomics & Ecosystem

24. **`Into<std::io::Error>` for `Error`** — Seamless `?` in io-returning functions.
25. **`Display` / `Debug` enrichment** — Richer debug output for `CmId`, `QueuePair`, `RdmaStream` (show QP num, state, addresses).
26. **New ibverbs API (Phase 3)** — `QueuePairEx`, `WrSession`, `CompletionQueueEx`, `PollGuard`. Skipped because siw doesn't support `ibv_create_cq_ex` / `ibv_qp_to_qp_ex`. Implement when rxe or hardware testing is available.

### From Gap Analysis

1. **XRC (Extended Reliable Connected)** — Reduces QP count in large-scale deployments by sharing receive-side resources across connections. No existing Rust library supports this.
2. **serde support** — Derive `Serialize`/`Deserialize` on `DeviceAttr`, `PortAttr`, GID, and QP endpoint info. Essential for out-of-band QP info exchange when not using rdma_cm.
3. **Multi-device / multi-port helpers** — Device lookup by name, GID, or subnet. Port selection and binding. `open_device_by_name()` exists but more is needed.
4. **UD-specific APIs** — Multicast group join/leave (`rdma_join_multicast`), send-with-AH pattern, UD QP address resolution.
5. **Tracing instrumentation** — `tracing` dependency added for Drop error reporting. Future: optional spans on resource creation/destruction and data-path operations (behind a feature flag).
6. **io_uring integration** — Use io_uring for CQ notification instead of epoll/completion channel. Potential for lower latency event loop integration.
7. **Connection pooling / reconnection** — Higher-level abstractions for managing multiple connections with automatic reconnect on failure.
8. ~~**Async runtime backends (tokio, smol)**~~ — **Tokio complete** (Phases A–E). Remaining for future: `SmolCqNotifier` (smol feature), async CM events. See [AsyncIntegration.md](AsyncIntegration.md) Phase F.
9. **Async device events** — `ibv_get_async_event` handling for port state changes, QP errors, and device removal notifications. Likely a `Context::async_events()` stream.
10. ~~**QP state transition helpers**~~ — **Partially done**: `to_init()`, `to_rtr()`, `to_rts()` exist. Future: higher-level `transition_to_rts(port, gid, remote_qpn, remote_psn)` that sets all attributes in one call.
