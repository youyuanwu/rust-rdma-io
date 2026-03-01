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

```rust
pub struct Context {
    inner: *mut ibv_context,
    // No parent Arc — Context is the root (device list freed at open time)
}

pub struct ProtectionDomain {
    inner: *mut ibv_pd,
    _ctx: Arc<Context>,     // Prevents context from being closed
}

pub struct CompletionQueue {
    inner: *mut ibv_cq,
    _ctx: Arc<Context>,
}

pub struct QueuePair {
    inner: *mut ibv_qp,
    _pd: Arc<ProtectionDomain>,
    _send_cq: Arc<CompletionQueue>,
    _recv_cq: Arc<CompletionQueue>,
    _srq: Option<Arc<SharedReceiveQueue>>,
}

pub struct MemoryRegion<'a> {
    inner: *mut ibv_mr,
    _pd: Arc<ProtectionDomain>,
    _buf: PhantomData<&'a mut [u8]>,  // Ties MR lifetime to buffer
}
```

### Why `Arc` and not lifetimes?

Pure lifetimes would be more zero-cost, but RDMA resource graphs are typically shared across threads (e.g., one thread posts sends, another polls CQ). `Arc` enables `Send + Sync` naturally. The overhead is one atomic increment per resource creation — negligible compared to kernel/driver round-trips.

### MemoryRegion is special

An MR pins a memory buffer for DMA access. The buffer **must not be freed or moved** while the MR is registered. Two approaches:

1. **Borrowed MR** (`MemoryRegion<'a>`) — ties MR lifetime to buffer lifetime via `PhantomData<&'a mut [u8]>`. Simple, but buffer must outlive all QP operations referencing it.
2. **Owned MR** (`OwnedMemoryRegion`) — MR owns its buffer (e.g., `Vec<u8>`). The buffer is freed when the MR is deregistered. Simpler lifetime management for long-lived buffers.

We provide both:

```rust
/// MR borrowing an external buffer.
pub struct MemoryRegion<'a> { ... }

/// MR owning its buffer.
pub struct OwnedMemoryRegion {
    inner: *mut ibv_mr,
    _pd: Arc<ProtectionDomain>,
    buf: Vec<u8>,
}
```

---

## 4. Core API Surface

### 4.1 Device & Context

```rust
/// List available RDMA devices.
pub fn devices() -> Result<Vec<Device>>

impl Device {
    pub fn name(&self) -> &str
    pub fn guid(&self) -> u64
    pub fn open(&self) -> Result<Arc<Context>>
}

impl Context {
    pub fn query_device(&self) -> Result<DeviceAttr>
    pub fn query_port(&self, port: u8) -> Result<PortAttr>
    pub fn query_gid(&self, port: u8, index: i32) -> Result<Gid>
    pub fn query_gid_table(&self) -> Result<Vec<GidEntry>>
    pub fn create_pd(self: &Arc<Self>) -> Result<Arc<ProtectionDomain>>
    pub fn create_cq(self: &Arc<Self>, cqe: i32) -> Result<Arc<CompletionQueue>>
    pub fn create_comp_channel(self: &Arc<Self>) -> Result<CompletionChannel>
}
```

### 4.2 Protection Domain

```rust
impl ProtectionDomain {
    pub fn reg_mr<'a>(
        self: &Arc<Self>,
        buf: &'a mut [u8],
        access: AccessFlags,
    ) -> Result<MemoryRegion<'a>>

    pub fn reg_mr_owned(
        self: &Arc<Self>,
        buf: Vec<u8>,
        access: AccessFlags,
    ) -> Result<OwnedMemoryRegion>

    pub fn create_qp(
        self: &Arc<Self>,
        init_attr: &QpInitAttr,
    ) -> Result<QueuePair>

    pub fn create_ah(
        self: &Arc<Self>,
        attr: &AhAttr,
    ) -> Result<AddressHandle>
}
```

### 4.3 Completion Queue

```rust
impl CompletionQueue {
    /// Poll for completions (legacy API). Returns number of completions.
    pub fn poll(&self, wc: &mut [WorkCompletion]) -> Result<usize>

    /// Request notification on next completion.
    pub fn req_notify(&self, solicited_only: bool) -> Result<()>
}

/// Extended CQ with the new polling API.
impl CompletionQueueEx {
    pub fn start_poll(&self) -> Result<PollGuard<'_>>
}

impl PollGuard<'_> {
    pub fn opcode(&self) -> WcOpcode
    pub fn byte_len(&self) -> u32
    pub fn qp_num(&self) -> u32
    pub fn status(&self) -> WcStatus
    pub fn next(&mut self) -> Result<bool>
    // ... other wc_read_* accessors
}
// PollGuard::drop calls ibv_end_poll
```

### 4.4 Queue Pair

```rust
pub struct QpInitAttr {
    pub send_cq: Arc<CompletionQueue>,
    pub recv_cq: Arc<CompletionQueue>,
    pub srq: Option<Arc<SharedReceiveQueue>>,
    pub cap: QpCap,
    pub qp_type: QpType,
    pub sq_sig_all: bool,
}

impl QueuePair {
    pub fn qp_num(&self) -> u32
    pub fn qp_type(&self) -> QpType

    /// Modify QP state (e.g., INIT → RTR → RTS).
    pub fn modify(&self, attr: &QpAttr, mask: QpAttrMask) -> Result<()>

    /// Post a send work request (legacy API).
    pub fn post_send(&self, wr: &SendWr) -> Result<()>

    /// Post a receive work request (legacy API).
    pub fn post_recv(&self, wr: &RecvWr) -> Result<()>
}

/// Extended QP with the new WR API (builder pattern).
impl QueuePairEx {
    pub fn start(&self) -> WrSession<'_>
}

impl WrSession<'_> {
    pub fn send(&mut self) -> &mut Self
    pub fn send_with_imm(&mut self, imm: u32) -> &mut Self
    pub fn rdma_write(&mut self, raddr: u64, rkey: u32) -> &mut Self
    pub fn rdma_read(&mut self, raddr: u64, rkey: u32) -> &mut Self
    pub fn atomic_fetch_add(&mut self, raddr: u64, rkey: u32, add: u64) -> &mut Self
    pub fn atomic_cmp_swap(&mut self, raddr: u64, rkey: u32, cmp: u64, swap: u64) -> &mut Self
    pub fn set_sge(&mut self, lkey: u32, addr: u64, len: u32) -> &mut Self
    pub fn set_inline_data(&mut self, data: &[u8]) -> &mut Self
    pub fn complete(self) -> Result<()>
    pub fn abort(self)
}
// WrSession::drop calls abort if not completed
```

### 4.5 Memory Region

```rust
impl<'a> MemoryRegion<'a> {
    pub fn lkey(&self) -> u32
    pub fn rkey(&self) -> u32
    pub fn addr(&self) -> *mut u8
    pub fn len(&self) -> usize
    pub fn as_slice(&self) -> &[u8]
    pub fn as_mut_slice(&mut self) -> &mut [u8]
}

impl OwnedMemoryRegion {
    pub fn lkey(&self) -> u32
    pub fn rkey(&self) -> u32
    pub fn as_slice(&self) -> &[u8]
    pub fn as_mut_slice(&mut self) -> &mut [u8]
}
```

### 4.6 Work Completion

```rust
/// Wrapper around ibv_wc with typed accessors.
#[repr(transparent)]
pub struct WorkCompletion(ibv_wc);

impl WorkCompletion {
    pub fn status(&self) -> WcStatus
    pub fn opcode(&self) -> WcOpcode
    pub fn byte_len(&self) -> u32
    pub fn qp_num(&self) -> u32
    pub fn wr_id(&self) -> u64
    pub fn imm_data(&self) -> Option<u32>
    pub fn is_success(&self) -> bool
}
```

---

## 5. RDMA CM (Connection Manager)

rdma_cm provides TCP-like connection semantics over RDMA. This is the primary way to use iWARP (siw) and the recommended approach for RoCE.

```rust
/// Event channel for receiving CM events.
pub struct EventChannel { ... }

/// Connection management ID — the core CM handle.
pub struct CmId { ... }

/// CM event received from an event channel.
pub struct CmEvent { ... }

impl EventChannel {
    pub fn new() -> Result<Self>
    pub fn get_event(&self) -> Result<CmEvent>
}

impl CmId {
    /// Create a new CM ID for active (client) or passive (server) use.
    pub fn new(channel: &EventChannel, port_space: PortSpace) -> Result<Self>

    /// Resolve destination address.
    pub fn resolve_addr(&self, src: Option<&SocketAddr>, dst: &SocketAddr, timeout_ms: i32) -> Result<()>

    /// Resolve route to destination.
    pub fn resolve_route(&self, timeout_ms: i32) -> Result<()>

    /// Connect to a remote peer (client side).
    pub fn connect(&self, param: &ConnParam) -> Result<()>

    /// Bind to local address and listen (server side).
    pub fn listen(&self, addr: &SocketAddr, backlog: i32) -> Result<()>

    /// Accept an incoming connection.
    pub fn accept(&self, param: &ConnParam) -> Result<()>

    /// Disconnect.
    pub fn disconnect(&self) -> Result<()>

    /// Access the underlying QP (created by rdma_cm).
    pub fn qp(&self) -> Option<&QueuePair>

    /// Access the PD (created automatically or user-supplied).
    pub fn pd(&self) -> Option<&ProtectionDomain>

    /// Create a QP associated with this CM ID.
    pub fn create_qp(&self, pd: &ProtectionDomain, init_attr: &QpInitAttr) -> Result<()>
}

impl CmEvent {
    pub fn event_type(&self) -> CmEventType
    pub fn status(&self) -> i32
    /// For CONNECT_REQUEST events, get the new CM ID for the incoming connection.
    pub fn new_id(&self) -> Option<CmId>
    pub fn ack(self)  // consumes event
}
```

### Connection flow (client)

```rust
let channel = EventChannel::new()?;
let id = CmId::new(&channel, PortSpace::Tcp)?;
id.resolve_addr(None, &"192.168.1.1:9999".parse()?, 2000)?;
let event = channel.get_event()?;  // ADDR_RESOLVED
event.ack();
id.resolve_route(2000)?;
let event = channel.get_event()?;  // ROUTE_RESOLVED
event.ack();
id.create_qp(&pd, &qp_attr)?;
id.connect(&conn_param)?;
let event = channel.get_event()?;  // ESTABLISHED
event.ack();
// ... data path ...
id.disconnect()?;
```

### Connection flow (server)

```rust
let channel = EventChannel::new()?;
let listener = CmId::new(&channel, PortSpace::Tcp)?;
listener.listen(&"0.0.0.0:9999".parse()?, 10)?;
loop {
    let event = channel.get_event()?;  // CONNECT_REQUEST
    let new_id = event.new_id().unwrap();
    event.ack();
    new_id.create_qp(&pd, &qp_attr)?;
    new_id.accept(&conn_param)?;
    let event = channel.get_event()?;  // ESTABLISHED
    event.ack();
    // ... handle connection ...
}
```

---

## 6. Async Completion Polling

> **Note**: Async runtime integration (tokio, smol) is deferred to future work. This section describes the design for when it is implemented. Phase 4 focuses on the core `AsyncCq` abstraction using raw fd polling (`rustix` / `mio`); runtime-specific backends will be added later.

### Design

RDMA completion notifications use file descriptors (via `ibv_comp_channel`). We register the fd with an event loop for edge-triggered readiness, then poll the CQ when notified.

```rust
/// Async-ready completion queue poller.
pub struct AsyncCq {
    cq: Arc<CompletionQueue>,
    channel: CompletionChannel,
    // fd-based async readiness
}

impl AsyncCq {
    pub fn new(cq: Arc<CompletionQueue>, channel: CompletionChannel) -> Result<Self>

    /// Wait for completions asynchronously.
    pub async fn poll(&self, wc: &mut [WorkCompletion]) -> Result<usize>

    /// Stream of completions.
    pub fn completions(&self) -> impl Stream<Item = Result<WorkCompletion>> + '_
}
```

### Completion notification flow

Each backend registers `comp_channel.fd` for read-readiness. When readable:
1. `ibv_get_cq_event()` to consume the notification
2. `ibv_req_notify_cq()` to re-arm
3. `ibv_poll_cq()` to drain completions

This is the standard CQ notification pattern — we just wire it into Rust async.

---

## 7. Stream Abstraction (Read / Write)

> **Note**: The async `RdmaStream` with `AsyncRead`/`AsyncWrite` depends on runtime integration (deferred). Phase 4 provides the synchronous stream abstraction using `std::io::Read`/`std::io::Write`. Async variants will be added when runtime backends land.

```rust
/// RDMA stream with Read + Write.
///
/// Uses rdma_cm for connection setup and RDMA SEND/RECV for data transfer.
/// Internally manages MR registration and work request posting.
pub struct RdmaStream {
    id: CmId,
    send_mr: OwnedMemoryRegion,
    recv_mr: OwnedMemoryRegion,
    // internal state: recv buffers, pending sends, etc.
}

impl RdmaStream {
    /// Connect to a remote RDMA endpoint.
    pub fn connect(addr: &SocketAddr) -> Result<Self>
}

impl std::io::Read for RdmaStream { ... }
impl std::io::Write for RdmaStream { ... }

/// RDMA listener (like TcpListener).
pub struct RdmaListener { ... }

impl RdmaListener {
    pub fn bind(addr: &SocketAddr) -> Result<Self>
    pub fn accept(&self) -> Result<RdmaStream>
}
```

### How it works

- **Write path**: Copy user data into pre-registered send buffer → post SEND WR → await completion
- **Read path**: Pre-post RECV WRs with registered buffers → await completion → copy to user buffer
- **Buffer management**: Double-buffering (one active, one being filled) to overlap I/O
- **Flow control**: Credit-based — sender tracks how many RECV buffers the remote has posted

This is an opinionated, high-level abstraction. Users who need fine-grained control use the lower-level QP/CQ APIs directly.

---

## 8. Error Handling

```rust
/// RDMA error types.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ibverbs call failed: {0}")]
    Verbs(#[source] std::io::Error),

    #[error("rdma_cm call failed: {0}")]
    Cm(#[source] std::io::Error),

    #[error("work completion error: status={status:?}, vendor_err={vendor_err}")]
    WorkCompletion {
        status: WcStatus,
        vendor_err: u32,
    },

    #[error("no RDMA devices found")]
    NoDevices,

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("invalid argument: {0}")]
    InvalidArg(String),
}

pub type Result<T> = std::result::Result<T, Error>;
```

Most ibverbs/rdmacm functions return 0 on success or set `errno`. The wrapper converts these to `std::io::Error` using `std::io::Error::last_os_error()`.

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
```

---

## 11. Enum / Flag Types

The FFI layer generates enums as `u32` type aliases with module-level constants. The safe API wraps these in proper Rust types:

```rust
/// QP transport type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpType {
    Rc,
    Uc,
    Ud,
    RawPacket,
    XrcSend,
    XrcRecv,
}

/// Memory region access flags.
bitflags::bitflags! {
    pub struct AccessFlags: u32 {
        const LOCAL_WRITE = IBV_ACCESS_LOCAL_WRITE;
        const REMOTE_WRITE = IBV_ACCESS_REMOTE_WRITE;
        const REMOTE_READ = IBV_ACCESS_REMOTE_READ;
        const REMOTE_ATOMIC = IBV_ACCESS_REMOTE_ATOMIC;
    }
}

/// QP attribute mask for ibv_modify_qp.
bitflags::bitflags! {
    pub struct QpAttrMask: i32 {
        const STATE = IBV_QP_STATE;
        const PORT = IBV_QP_PORT;
        const PKEY_INDEX = IBV_QP_PKEY_INDEX;
        const ACCESS_FLAGS = IBV_QP_ACCESS_FLAGS;
        // ...
    }
}
```

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
| Async approach | Deferred — core uses sync spin-polling | Runtime integration (tokio/smol) is future work |
| Error handling | `thiserror` + `std::io::Error` | Composable with Rust I/O ecosystem |
| Drop error handling | `tracing::error!` on all resource destruction failures | Silent drops hide kernel resource leaks |
| Enum wrapping | `bitflags` for flags, Rust enums for types | Type safety without runtime cost |
| API surface | Both legacy and new ibverbs | Legacy for compatibility, new for performance |
| Connection mgmt | rdma_cm as primary | Required for iWARP, recommended for RoCE |
| Stream abstraction | `std::io::Read`/`Write` first | Sync first, async (`AsyncRead`/`AsyncWrite`) when runtime backends land |
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
12. **`CompletionChannel`** — Wrap `ibv_comp_channel` for event-driven CQ notification. Prerequisite for async backends.
13. **`qp_type()` accessor** — Return the transport type of a `QueuePair`.
14. **`CmId.qp()` / `CmId.pd()` safe accessors** — Return `&QueuePair` / `&ProtectionDomain` instead of raw pointers.
15. **`query_gid_table()`** — Bulk GID table query for device discovery.
16. **Feature flags** — §10 design specifies `legacy-api`, `new-api`, `cm`, `stream` features but none are implemented; everything compiles unconditionally.

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
8. **Async runtime backends (tokio, smol)** — `AsyncFd` (tokio) or `Async` (async-io/smol) wrappers around the comp_channel fd, enabling `AsyncCq`, `AsyncRead`/`AsyncWrite` on `RdmaStream`, and runtime-agnostic async CQ polling.
9. **Async device events** — `ibv_get_async_event` handling for port state changes, QP errors, and device removal notifications. Likely a `Context::async_events()` stream.
10. ~~**QP state transition helpers**~~ — **Partially done**: `to_init()`, `to_rtr()`, `to_rts()` exist. Future: higher-level `transition_to_rts(port, gid, remote_qpn, remote_psn)` that sets all attributes in one call.
