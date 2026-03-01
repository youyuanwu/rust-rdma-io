# RDMA Async Integration — Design

> Design for integrating `rdma-io` with Rust async runtimes (tokio, smol).

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
- Connection-level async (`rdma_cm` event channel) — separate from CQ async
- Direct `tokio::io::AsyncRead` impl — users use `tokio_util::compat::FuturesAsyncReadCompatExt` instead

---

## 3. Architecture

### Layered design

```
┌─────────────────────────────────────────────────┐
│  AsyncRdmaStream / AsyncRdmaListener            │  ← High-level stream (§5)
│  (futures::io::AsyncRead + AsyncWrite)           │    SEND/RECV with buffering
├────────────────────────┬────────────────────────┤
│  AsyncQp               │                        │  ← Mid-level verb access (§7)
│  send() recv()         │  (AsyncRdmaStream uses │    All verbs: READ/WRITE/
│  read_remote()         │   AsyncQp internally)  │    SEND/RECV/Atomics
│  write_remote()        │                        │
│  compare_and_swap()    │                        │
├────────────────────────┴────────────────────────┤
│  AsyncCq                                        │  ← Async CQ poller (core)
│  (runtime-agnostic via CqNotifier trait)        │
├──────────────────┬──────────────────────────────┤
│  TokioCqNotifier │  SmolCqNotifier              │  ← Runtime adapters (feature-gated)
│  (AsyncFd)       │  (async_io::Async)           │
├──────────────────┴──────────────────────────────┤
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
│   ├── async_stream.rs       # AsyncRdmaStream / AsyncRdmaListener
│   ├── notifier/
│   │   ├── mod.rs
│   │   ├── tokio.rs          # TokioCqNotifier (behind "tokio" feature)
│   │   └── smol.rs           # SmolCqNotifier (behind "smol" feature)
│   └── ... (existing modules)
└── Cargo.toml
```

### Feature flags

```toml
[features]
default = []

# Async runtime support
tokio = ["dep:tokio"]
smol = ["dep:async-io"]

[dependencies]
tokio = { version = "1", features = ["net"], optional = true }
async-io = { version = "2", optional = true }
```

---

## 4. Core Types

### 4.1 CompletionChannel

Safe wrapper around `ibv_comp_channel`. Prerequisite for all async CQ operations.

```rust
use std::os::unix::io::{AsRawFd, RawFd};

/// Safe wrapper around `ibv_comp_channel`.
///
/// Provides the file descriptor used for async CQ notification.
/// The fd becomes readable when a CQ associated with this channel
/// has a completion event.
pub struct CompletionChannel {
    inner: *mut ibv_comp_channel,
    _ctx: Arc<Context>,
}

impl CompletionChannel {
    /// Create a new completion channel.
    pub fn new(ctx: &Arc<Context>) -> Result<Self> {
        let ch = from_ptr(unsafe { ibv_create_comp_channel(ctx.inner) })?;
        // Set non-blocking for async use
        set_nonblocking(unsafe { (*ch).fd })?;
        Ok(Self { inner: ch, _ctx: Arc::clone(ctx) })
    }

    /// Raw file descriptor (for async reactor registration).
    pub fn fd(&self) -> RawFd {
        unsafe { (*self.inner).fd }
    }

    /// Raw pointer (for CQ creation).
    pub(crate) fn as_raw(&self) -> *mut ibv_comp_channel {
        self.inner
    }

    /// Consume one CQ event notification (non-blocking).
    ///
    /// Returns the CQ that fired. Caller must call `ack_cq_events` later.
    pub fn get_cq_event(&self) -> Result<(*mut ibv_cq, usize)> {
        let mut cq: *mut ibv_cq = std::ptr::null_mut();
        let mut ctx: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            ibv_get_cq_event(self.inner, &mut cq, &mut ctx)
        };
        if ret != 0 {
            Err(Error::Verbs(io::Error::last_os_error()))
        } else {
            Ok((cq, 1))
        }
    }
}

impl AsRawFd for CompletionChannel {
    fn as_raw_fd(&self) -> RawFd { self.fd() }
}

impl Drop for CompletionChannel {
    fn drop(&mut self) {
        let ret = unsafe { ibv_destroy_comp_channel(self.inner) };
        if ret != 0 {
            tracing::error!("ibv_destroy_comp_channel failed: {}",
                io::Error::from_raw_os_error(-ret));
        }
    }
}
```

### 4.2 CqNotifier trait (runtime abstraction)

```rust
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Trait abstracting over async runtimes for CQ fd readiness.
///
/// Each runtime provides an implementation that registers the
/// comp_channel fd with its reactor and awaits readiness.
pub trait CqNotifier: Send + Sync {
    /// Wait until the comp_channel fd is readable.
    ///
    /// Returns when the fd has data (a CQ event notification).
    /// The caller must consume the event and re-arm.
    fn readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>>;
}
```

### 4.3 Tokio adapter

```rust
#[cfg(feature = "tokio")]
pub mod tokio_notifier {
    use tokio::io::unix::AsyncFd;
    use std::os::unix::io::RawFd;

    pub struct TokioCqNotifier {
        async_fd: AsyncFd<RawFd>,
    }

    impl TokioCqNotifier {
        pub fn new(fd: RawFd) -> io::Result<Self> {
            Ok(Self {
                async_fd: AsyncFd::new(fd)?,
            })
        }
    }

    impl CqNotifier for TokioCqNotifier {
        fn readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async {
                let mut guard = self.async_fd.readable().await?;
                guard.clear_ready();
                Ok(())
            })
        }
    }
}
```

### 4.4 Smol adapter

```rust
#[cfg(feature = "smol")]
pub mod smol_notifier {
    use async_io::Async;
    use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd, RawFd};

    /// Wrapper to implement AsFd for a raw fd.
    struct FdWrapper(OwnedFd);

    pub struct SmolCqNotifier {
        async_fd: Async<FdWrapper>,
    }

    impl SmolCqNotifier {
        pub fn new(fd: RawFd) -> io::Result<Self> {
            // Duplicate the fd so Async owns it independently
            let owned = unsafe { OwnedFd::from_raw_fd(libc::dup(fd)) };
            Ok(Self {
                async_fd: Async::new(FdWrapper(owned))?,
            })
        }
    }

    impl CqNotifier for SmolCqNotifier {
        fn readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async {
                self.async_fd.readable().await?;
                Ok(())
            })
        }
    }
}
```

### 4.5 AsyncCq — the core async CQ poller

```rust
/// Async completion queue poller.
///
/// Wraps a `CompletionQueue` + `CompletionChannel` + runtime `CqNotifier`
/// to provide async-await CQ polling without spin loops.
///
/// # Completion notification protocol
///
/// Uses the standard drain-after-arm pattern:
/// 1. `req_notify_cq()` — arm CQ notification
/// 2. `poll_cq()` — drain any completions that arrived before arming
/// 3. If completions found → return them (skip waiting)
/// 4. If no completions → `notifier.readable().await` (sleep until fd fires)
/// 5. `get_cq_event()` + `ack_cq_events()` — consume notification
/// 6. Go to 1
pub struct AsyncCq {
    cq: Arc<CompletionQueue>,
    channel: CompletionChannel,
    notifier: Box<dyn CqNotifier>,
    /// Accumulated unacked CQ events (must ack before destroy).
    unacked_events: AtomicU32,
}

impl AsyncCq {
    pub fn new(
        cq: Arc<CompletionQueue>,
        channel: CompletionChannel,
        notifier: Box<dyn CqNotifier>,
    ) -> Self {
        Self { cq, channel, notifier, unacked_events: AtomicU32::new(0) }
    }

    /// Poll for up to `wc_buf.len()` completions asynchronously.
    ///
    /// Returns when at least one completion is available.
    pub async fn poll(&self, wc_buf: &mut [WorkCompletion]) -> Result<usize> {
        loop {
            // 1. Arm notification
            self.cq.req_notify(false)?;

            // 2. Drain any completions (catches race between arm and await)
            let n = self.cq.poll(wc_buf)?;
            if n > 0 {
                return Ok(n);
            }

            // 3. No completions — wait for fd readiness
            self.notifier.readable().await
                .map_err(Error::Verbs)?;

            // 4. Consume the CQ event
            let _ = self.channel.get_cq_event()?;
            self.unacked_events.fetch_add(1, Ordering::Relaxed);

            // 5. Ack periodically to avoid overflow
            let unacked = self.unacked_events.load(Ordering::Relaxed);
            if unacked >= 16 {
                unsafe { ibv_ack_cq_events(self.cq.as_raw(), unacked) };
                self.unacked_events.store(0, Ordering::Relaxed);
            }

            // 6. Loop back — poll will find completions now
        }
    }

    /// Wait for a specific WR ID completion.
    pub async fn poll_wr_id(&self, expected: u64) -> Result<WorkCompletion> {
        let mut wc = [WorkCompletion::default(); 4];
        loop {
            let n = self.poll(&mut wc).await?;
            for item in &wc[..n] {
                if item.wr_id() == expected {
                    return Ok(*item);
                }
            }
        }
    }
}

impl Drop for AsyncCq {
    fn drop(&mut self) {
        // Ack all remaining events before CQ destruction
        let unacked = self.unacked_events.load(Ordering::Relaxed);
        if unacked > 0 {
            unsafe { ibv_ack_cq_events(self.cq.as_raw(), unacked) };
        }
    }
}
```

---

## 5. AsyncRdmaStream

### Design

`AsyncRdmaStream` mirrors `RdmaStream` but uses `AsyncCq` instead of spin-polling.

```rust
/// Async RDMA stream implementing `AsyncRead` + `AsyncWrite`.
///
/// # Example (tokio)
///
/// ```no_run
/// use rdma_io::async_stream::{AsyncRdmaStream, AsyncRdmaListener};
/// use futures::io::{AsyncReadExt, AsyncWriteExt};
/// // For tokio compat: use tokio_util::compat::FuturesAsyncReadCompatExt;
///
/// #[tokio::main]
/// async fn main() -> rdma_io::Result<()> {
///     let mut stream = AsyncRdmaStream::connect_tokio(
///         &"10.0.0.1:9999".parse().unwrap()
///     ).await?;
///     stream.write_all(b"hello async rdma").await?;
///     let mut buf = [0u8; 1024];
///     let n = stream.read(&mut buf).await?;
///     Ok(())
/// }
/// ```
pub struct AsyncRdmaStream {
    _event_channel: EventChannel,
    cm_id: CmId,
    _pd: Arc<ProtectionDomain>,
    async_cq: AsyncCq,
    send_mr: OwnedMemoryRegion,
    recv_mrs: [OwnedMemoryRegion; NUM_RECV_BUFS],
    recv_pending: Option<(usize, usize, usize)>,
}
```

### Connection setup

Connection setup (`resolve_addr`, `resolve_route`, `connect`) is inherently sequential and fast. We keep it synchronous and wrap in `spawn_blocking` if needed:

```rust
impl AsyncRdmaStream {
    /// Connect using tokio runtime.
    #[cfg(feature = "tokio")]
    pub async fn connect_tokio(addr: &SocketAddr) -> Result<Self> {
        let addr = *addr;
        // CM setup is fast (<100ms) but does block on kernel calls
        tokio::task::spawn_blocking(move || {
            Self::connect_sync(&addr, |fd| {
                Box::new(TokioCqNotifier::new(fd)?) as Box<dyn CqNotifier>
            })
        }).await.map_err(|e| Error::InvalidArg(e.to_string()))?
    }

    /// Connect using smol runtime.
    #[cfg(feature = "smol")]
    pub async fn connect_smol(addr: &SocketAddr) -> Result<Self> {
        let addr = *addr;
        smol::unblock(move || {
            Self::connect_sync(&addr, |fd| {
                Box::new(SmolCqNotifier::new(fd)?) as Box<dyn CqNotifier>
            })
        }).await
    }

    /// Internal sync connect that accepts a notifier factory.
    fn connect_sync(
        addr: &SocketAddr,
        make_notifier: impl FnOnce(RawFd) -> io::Result<Box<dyn CqNotifier>>,
    ) -> Result<Self> {
        let ch = EventChannel::new()?;
        let cm_id = CmId::new(&ch, PortSpace::Tcp)?;

        cm_id.resolve_addr(None, addr, 2000)?;
        expect_event(&ch, CmEventType::AddrResolved)?.ack();
        cm_id.resolve_route(2000)?;
        expect_event(&ch, CmEventType::RouteResolved)?.ack();

        let ctx = cm_id.verbs_context().ok_or(Error::InvalidArg("no ctx".into()))?;
        let pd = cm_id.alloc_pd()?;

        // Create CQ with completion channel for async notification
        let comp_channel = CompletionChannel::new(&ctx)?;
        let cq = CompletionQueue::with_comp_channel(
            Arc::clone(&ctx), 32, comp_channel.as_raw()
        )?;
        let notifier = make_notifier(comp_channel.fd())?;
        let async_cq = AsyncCq::new(Arc::clone(&cq), comp_channel, notifier);

        cm_id.create_qp_with_cq(&pd, &stream_qp_attr(), Some(&cq), Some(&cq))?;

        // ... register MRs, post recv, connect (same as sync) ...
    }
}
```

### AsyncRead / AsyncWrite (futures::io — runtime-agnostic)

We implement `futures::io::AsyncRead` + `AsyncWrite` as the **primary** trait implementation.
This works natively with smol/async-std, and tokio users wrap with `.compat()`:

```rust
use tokio_util::compat::FuturesAsyncReadCompatExt;
let stream = AsyncRdmaStream::connect(...).await?;
let compat = stream.compat(); // now implements tokio::io::AsyncRead
tokio::io::copy(&mut compat, &mut sink).await?;
```

```rust
impl futures::io::AsyncRead for AsyncRdmaStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // If recv_pending has buffered data, return it immediately
        // Otherwise, poll the AsyncCq for a recv completion
        // Pin the async_cq.poll() future and drive it with cx
    }
}

impl futures::io::AsyncWrite for AsyncRdmaStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Copy to send_mr, post SEND WR
        // Poll AsyncCq for send completion
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // rdma_disconnect
        Poll::Ready(Ok(()))
    }
}
```

### Implementing `poll_read` / `poll_write` correctly

The challenge: `AsyncRead::poll_read` must return `Poll::Pending` and register with the waker, but our `AsyncCq::poll()` is an async fn. We need to store the future across poll calls:

```rust
/// Internal state machine for pending async operations.
enum ReadState {
    /// No read in progress — need to start one.
    Idle,
    /// Waiting for CQ completion.
    Polling(Pin<Box<dyn Future<Output = Result<usize>> + Send>>),
}

impl AsyncRdmaStream {
    fn poll_read_inner(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // Check buffered recv data first
        if let Some((buf_idx, offset, total_len)) = self.recv_pending {
            // ... copy from recv buffer, same as sync ...
            return Poll::Ready(Ok(copy_len));
        }

        // Start or resume CQ polling
        match &mut self.read_state {
            ReadState::Idle => {
                // Start polling
                let mut wc = [WorkCompletion::default(); 4];
                let future = self.async_cq.poll(&mut wc);
                self.read_state = ReadState::Polling(Box::pin(future));
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            ReadState::Polling(ref mut fut) => {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(n)) => {
                        // Process completions, copy to user buffer
                        self.read_state = ReadState::Idle;
                        Poll::Ready(Ok(copy_len))
                    }
                    Poll::Ready(Err(e)) => {
                        self.read_state = ReadState::Idle;
                        Poll::Ready(Err(io::Error::other(e)))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}
```

---

## 6. Alternative: Simpler Approach via `poll_fn`

Instead of implementing `AsyncRead`/`AsyncWrite` directly (complex state machines), we can provide simpler async methods that users call:

```rust
impl AsyncRdmaStream {
    /// Read data asynchronously.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Check buffered recv data
        if let Some((buf_idx, offset, total_len)) = self.recv_pending.take() {
            // ... return buffered data ...
        }

        // Wait for recv completion
        let mut wc = [WorkCompletion::default(); 4];
        loop {
            let n = self.async_cq.poll(&mut wc).await
                .map_err(io::Error::other)?;
            for item in &wc[..n] {
                let wr_id = item.wr_id();
                if wr_id < NUM_RECV_BUFS as u64 {
                    // Process recv completion, copy to buf
                    return Ok(copy_len);
                }
                // Ignore send completions
            }
        }
    }

    /// Write data asynchronously.
    pub async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Copy to send_mr, post SEND
        // Wait for send completion
        self.async_cq.poll_wr_id(SEND_WR_ID).await
            .map_err(io::Error::other)?;
        Ok(send_len)
    }
}
```

This is simpler and can be wrapped into `AsyncRead`/`AsyncWrite` via `futures_util::io::AsyncReadExt` adapters or tokio's `StreamReader` (via compat layer).

**Recommendation**: Start with the simpler async methods (§6), add trait impls (§5) as a follow-up.

---

## 7. Async RDMA Verbs (Individual Operations)

Beyond the stream abstraction (SEND/RECV), RDMA supports one-sided operations (READ/WRITE) and atomics that don't involve the remote CPU. These are the core value proposition of RDMA and should have first-class async support.

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

The central type for async verb operations. Wraps a `QueuePair` + `AsyncCq` and provides an async method per verb:

```rust
/// Async wrapper around a QueuePair for individual RDMA verb operations.
///
/// Unlike `AsyncRdmaStream` (which provides a stream abstraction over SEND/RECV),
/// `AsyncQp` exposes each RDMA verb individually, giving full control over
/// one-sided and two-sided operations.
pub struct AsyncQp {
    qp: QueuePair,
    async_cq: AsyncCq,       // shared send + recv CQ (or separate)
    // Optional: separate send/recv CQs
    // async_send_cq: AsyncCq,
    // async_recv_cq: AsyncCq,
}
```

### 7.3 Two-sided verbs (SEND / RECV)

```rust
impl AsyncQp {
    /// Post a SEND and await its completion.
    pub async fn send(&self, mr: &MemoryRegion, range: Range<usize>, wr_id: u64) -> Result<()> {
        let mut wr = SendWr::new(mr, range, wr_id, ibv_wr_opcode::IBV_WR_SEND);
        self.qp.post_send(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }

    /// Post a RECV buffer and await its completion. Returns bytes received.
    pub async fn recv(&self, mr: &mut MemoryRegion, range: Range<usize>, wr_id: u64) -> Result<u32> {
        let mut wr = RecvWr::new(mr, range, wr_id);
        self.qp.post_recv(&mut wr)?;
        let wc = self.async_cq.poll_wr_id(wr_id).await?;
        Ok(wc.byte_len())
    }

    /// Post a SEND with immediate data.
    pub async fn send_with_imm(
        &self, mr: &MemoryRegion, range: Range<usize>, wr_id: u64, imm: u32,
    ) -> Result<()> {
        let mut wr = SendWr::new(mr, range, wr_id, ibv_wr_opcode::IBV_WR_SEND_WITH_IMM);
        wr.set_imm_data(imm);
        self.qp.post_send(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }
}
```

### 7.4 One-sided verbs (RDMA READ / WRITE)

One-sided operations access remote memory directly without involving the remote CPU. They require a `RemoteMemoryRegion` token (rkey + remote address) exchanged out-of-band.

```rust
/// Token representing a remote memory region.
/// Exchanged between peers during setup (e.g., via SEND/RECV or a side channel).
#[derive(Debug, Clone, Copy)]
pub struct RemoteMr {
    pub addr: u64,
    pub len: u32,
    pub rkey: u32,
}

impl AsyncQp {
    /// RDMA READ: read from remote memory into a local MR.
    /// The remote side is not notified.
    pub async fn read_remote(
        &self,
        local_mr: &mut MemoryRegion,
        local_range: Range<usize>,
        remote: &RemoteMr,
        remote_offset: usize,
        wr_id: u64,
    ) -> Result<()> {
        let mut wr = SendWr::new(local_mr, local_range, wr_id, ibv_wr_opcode::IBV_WR_RDMA_READ);
        wr.set_remote(remote.addr + remote_offset as u64, remote.rkey);
        self.qp.post_send(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }

    /// RDMA WRITE: write from local MR to remote memory.
    /// The remote side is not notified.
    pub async fn write_remote(
        &self,
        local_mr: &MemoryRegion,
        local_range: Range<usize>,
        remote: &RemoteMr,
        remote_offset: usize,
        wr_id: u64,
    ) -> Result<()> {
        let mut wr = SendWr::new(local_mr, local_range, wr_id, ibv_wr_opcode::IBV_WR_RDMA_WRITE);
        wr.set_remote(remote.addr + remote_offset as u64, remote.rkey);
        self.qp.post_send(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }

    /// RDMA WRITE with immediate data.
    /// Remote side receives a RECV completion with the imm_data.
    pub async fn write_remote_with_imm(
        &self,
        local_mr: &MemoryRegion,
        local_range: Range<usize>,
        remote: &RemoteMr,
        remote_offset: usize,
        imm: u32,
        wr_id: u64,
    ) -> Result<()> {
        let mut wr = SendWr::new(local_mr, local_range, wr_id, ibv_wr_opcode::IBV_WR_RDMA_WRITE_WITH_IMM);
        wr.set_remote(remote.addr + remote_offset as u64, remote.rkey);
        wr.set_imm_data(imm);
        self.qp.post_send(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await
    }
}
```

### 7.5 Atomic verbs

Atomic operations require the QP to be `RC` (reliable connected) and the remote MR to have `IBV_ACCESS_REMOTE_ATOMIC`. The target address must be 8-byte aligned.

```rust
impl AsyncQp {
    /// Compare-and-swap: if remote value == `expected`, set to `desired`.
    /// Returns the original remote value (before the swap attempt).
    pub async fn compare_and_swap(
        &self,
        local_mr: &mut MemoryRegion,  // 8 bytes to receive old value
        local_offset: usize,
        remote: &RemoteMr,
        remote_offset: usize,
        expected: u64,
        desired: u64,
        wr_id: u64,
    ) -> Result<u64> {
        let mut wr = SendWr::new_atomic(
            local_mr, local_offset, wr_id,
            ibv_wr_opcode::IBV_WR_ATOMIC_CMP_AND_SWP,
        );
        wr.set_remote(remote.addr + remote_offset as u64, remote.rkey);
        wr.set_atomic(expected, desired);
        self.qp.post_send(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await?;
        // Old value is written into local_mr at local_offset
        Ok(u64::from_ne_bytes(local_mr.as_slice()[local_offset..local_offset+8].try_into().unwrap()))
    }

    /// Fetch-and-add: atomically adds `add_val` to the remote 8-byte value.
    /// Returns the original remote value (before the add).
    pub async fn fetch_and_add(
        &self,
        local_mr: &mut MemoryRegion,  // 8 bytes to receive old value
        local_offset: usize,
        remote: &RemoteMr,
        remote_offset: usize,
        add_val: u64,
        wr_id: u64,
    ) -> Result<u64> {
        let mut wr = SendWr::new_atomic(
            local_mr, local_offset, wr_id,
            ibv_wr_opcode::IBV_WR_ATOMIC_FETCH_AND_ADD,
        );
        wr.set_remote(remote.addr + remote_offset as u64, remote.rkey);
        wr.set_atomic(add_val, 0);  // compare_add = add_val, swap = unused
        self.qp.post_send(&mut wr)?;
        self.async_cq.poll_wr_id(wr_id).await?;
        Ok(u64::from_ne_bytes(local_mr.as_slice()[local_offset..local_offset+8].try_into().unwrap()))
    }
}
```

### 7.6 Batch operations

For high-throughput use cases, posting many WRs one at a time is inefficient. We support posting a batch and awaiting all completions:

```rust
impl AsyncQp {
    /// Post multiple send WRs as a linked list and await all completions.
    pub async fn post_send_batch(&self, wrs: &mut [SendWr]) -> Result<Vec<WorkCompletion>> {
        // Link WRs into a chain (each wr.next points to the next)
        for i in 0..wrs.len() - 1 {
            wrs[i].set_next(&mut wrs[i + 1]);
        }
        self.qp.post_send(&mut wrs[0])?;
        self.async_cq.poll_n(wrs.len()).await
    }
}
```

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
│  poll() poll_wr_id()                              │  Runtime-agnostic
├──────────────────────────────────────────────────┤
│  QueuePair / CompletionQueue / MemoryRegion       │  Sync core (existing)
└──────────────────────────────────────────────────┘
```

- **`AsyncRdmaStream`** uses `AsyncQp` internally (or shares the same `AsyncCq`) for SEND/RECV with buffering, flow control, and `AsyncRead`/`AsyncWrite` traits.
- **`AsyncQp`** gives direct access to all verbs — users manage MRs, WR IDs, and flow control themselves. This is the right choice for RDMA READ/WRITE workloads, key-value stores, distributed locks, etc.
- Both share the **`AsyncCq`** layer for async completion handling.

### 7.8 Sync verb equivalents

The same API pattern works synchronously (already partially exists via `QueuePair::post_send`). We'll add ergonomic sync wrappers that post + poll in one call:

```rust
impl QueuePair {
    /// Synchronous RDMA READ: post + spin-poll until completion.
    pub fn read_remote_sync(
        &self, cq: &CompletionQueue,
        local_mr: &mut MemoryRegion, local_range: Range<usize>,
        remote: &RemoteMr, remote_offset: usize,
    ) -> Result<()> {
        let wr_id = /* unique id */;
        let mut wr = SendWr::new(local_mr, local_range, wr_id, ibv_wr_opcode::IBV_WR_RDMA_READ);
        wr.set_remote(remote.addr + remote_offset as u64, remote.rkey);
        self.post_send(&mut wr)?;
        cq.poll_blocking_wr_id(wr_id)
    }
}
```

### 7.9 Prerequisites

Before `AsyncQp` can be implemented, the sync layer needs:

1. **`SendWr` builder methods**: `set_remote(addr, rkey)`, `set_imm_data(imm)`, `set_atomic(compare_add, swap)`, `set_next(wr)`
2. **`RemoteMr` type**: simple struct with `addr`, `len`, `rkey` — exchanged out-of-band
3. **`WorkCompletion` accessors**: `byte_len()`, `imm_data()`, `opcode()`
4. **`MemoryRegion::rkey()`** and **`MemoryRegion::addr()`** — expose remote access tokens
5. **MR access flags**: `IBV_ACCESS_REMOTE_READ`, `REMOTE_WRITE`, `REMOTE_ATOMIC` on registration

---

## 8. Async CM Event Channel (Connection Setup)

The `rdma_event_channel` struct has an `.fd` field — a regular Linux file descriptor, just like `ibv_comp_channel`. This means **the entire connection flow can be fully async**, not just the data path.

### How it works

```
rdma_event_channel {
    pub fd: i32,   // ← pollable with epoll/kqueue, wrappable with AsyncFd
}
```

The fd becomes readable whenever `rdma_get_cm_event()` has an event to return. By setting the fd to non-blocking and registering it with the async reactor, all connection management operations become non-blocking:

```rust
/// Async wrapper around rdma_event_channel.fd.
pub struct AsyncEventChannel {
    channel: EventChannel,
    notifier: Box<dyn CqNotifier>,  // reuse the same CqNotifier trait
}

impl AsyncEventChannel {
    /// Wait for the next CM event asynchronously.
    pub async fn get_event(&self) -> Result<CmEvent> {
        loop {
            self.notifier.readable().await.map_err(Error::Verbs)?;
            match self.channel.try_get_event() {
                Ok(ev) => return Ok(ev),
                Err(e) if e.would_block() => continue,  // spurious wakeup
                Err(e) => return Err(e),
            }
        }
    }
}
```

### Fully async connection flow

```rust
// Client — fully async, no blocking calls
impl AsyncRdmaStream {
    pub async fn connect(addr: &SocketAddr) -> Result<Self> {
        let async_ch = AsyncEventChannel::new_tokio()?;
        let cm_id = CmId::new(async_ch.inner(), PortSpace::Tcp)?;

        cm_id.resolve_addr(None, addr, 2000)?;
        let ev = async_ch.get_event().await?;         // async wait for ADDR_RESOLVED
        assert_eq!(ev.event_type(), CmEventType::AddrResolved);
        ev.ack();

        cm_id.resolve_route(2000)?;
        let ev = async_ch.get_event().await?;         // async wait for ROUTE_RESOLVED
        ev.ack();

        // ... create QP, MRs ...
        cm_id.connect(&param)?;
        let ev = async_ch.get_event().await?;         // async wait for ESTABLISHED
        ev.ack();

        Ok(stream)
    }
}

// Server — async accept loop
impl AsyncRdmaListener {
    pub async fn accept(&self) -> Result<AsyncRdmaStream> {
        let ev = self.async_ch.get_event().await?;    // async wait for CONNECT_REQUEST
        let conn_id = unsafe { CmId::from_raw(ev.cm_id_raw(), true) };
        ev.ack();

        // ... create QP, accept ...
        conn_id.accept(&param)?;
        let ev = self.async_ch.get_event().await?;    // async wait for ESTABLISHED
        ev.ack();

        Ok(stream)
    }
}
```

### Advantages over `spawn_blocking`

| Approach | Pros | Cons |
|----------|------|------|
| `spawn_blocking` (§5 current design) | Simple, works today | Ties up a thread pool thread for ~10ms |
| Async CM events (this section) | True async, no thread pool | More code, need non-blocking EventChannel |

**Recommendation**: Phase B (§11) uses `spawn_blocking` for simplicity. Phase E adds fully async CM events. For servers accepting many connections concurrently, async CM is important to avoid thread pool exhaustion.

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

`ibv_ack_cq_events` is expensive (takes a mutex internally). Batch acknowledgements:

- Ack every 16 events (configurable)
- Ack all remaining in `Drop`
- This is already handled in our `AsyncCq` design (§4.5)

---

## 10. Testing Strategy

### Unit tests (no RDMA device)

- `CqNotifier` trait with a mock implementation
- Verify drain-after-arm loop logic
- Verify event acking and batching

### Integration tests (siw)

```rust
#[tokio::test]
async fn async_stream_echo() {
    let listener = AsyncRdmaListener::bind_tokio(&bind_addr).await.unwrap();
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        stream.write(&buf[..n]).await.unwrap();
    });

    let mut client = AsyncRdmaStream::connect_tokio(&connect_addr).await.unwrap();
    client.write(b"hello async").await.unwrap();
    let mut buf = [0u8; 1024];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello async");

    server.await.unwrap();
}
```

### Compatibility test matrix

| Test | tokio | smol | Notes |
|------|-------|------|-------|
| async_echo | ✅ | ✅ | Basic round-trip |
| async_multi_message | ✅ | ✅ | 5 round-trips |
| async_concurrent_streams | ✅ | ✅ | Multiple connections on one runtime |
| async_interleave_with_tcp | ✅ | ✅ | RDMA + TCP on same event loop |
| async_cancellation | ✅ | ✅ | Drop future mid-read |

---

## 11. Implementation Phases

### Phase A: CompletionChannel + AsyncCq

1. Implement `CompletionChannel` (safe `ibv_comp_channel` wrapper)
2. Re-add `CompletionQueue::with_comp_channel` constructor
3. Implement `CqNotifier` trait
4. Implement `TokioCqNotifier` (tokio feature)
5. Implement `AsyncCq` with drain-after-arm loop
6. Test: create async CQ, post WR, await completion

### Phase B: AsyncRdmaStream (simple async methods)

1. `AsyncRdmaStream::connect_tokio()` / `connect_smol()`
2. `AsyncRdmaStream::read()` / `write()` (async methods, not trait impls)
3. `AsyncRdmaListener::bind_tokio()` / `accept()`
4. Tests: echo, multi-message

### Phase C: AsyncRead / AsyncWrite trait impls

1. Internal state machine for `poll_read` / `poll_write`
2. Implement `futures::io::AsyncRead` + `AsyncWrite` (primary, runtime-agnostic)
3. Tokio users use `tokio_util::compat::FuturesAsyncReadCompatExt` — no separate impl needed
4. Tests: verify compat layer works with `tokio::io::copy`, `BufReader`, etc.

### Phase D: AsyncQp — Async RDMA Verbs

1. Sync prerequisites: `SendWr` builder methods, `RemoteMr` type, MR access flag support
2. `AsyncQp` struct wrapping `QueuePair` + `AsyncCq`
3. Two-sided: `send()`, `recv()`, `send_with_imm()`
4. One-sided: `read_remote()`, `write_remote()`, `write_remote_with_imm()`
5. Atomics: `compare_and_swap()`, `fetch_and_add()`
6. Batch: `post_send_batch()`
7. Sync convenience wrappers: `QueuePair::read_remote_sync()`, etc.
8. Tests: RDMA READ/WRITE roundtrip, atomic CAS, batch operations (requires siw or rxe)

### Phase E: SmolCqNotifier + CM event async

1. `SmolCqNotifier` (smol feature)
2. Async CM event channel (for `AsyncRdmaListener::accept`)
3. Full test matrix across both runtimes

---

## 12. Dependencies

| Crate | Version | Feature | Purpose |
|-------|---------|---------|---------|
| `futures-io` | 0.3 | (always, when async) | `AsyncRead`/`AsyncWrite` traits (primary, runtime-agnostic) |
| `tokio` | 1.x | `tokio` | `AsyncFd`, `spawn_blocking` |
| `tokio-util` | 0.7 | `tokio` | `FuturesAsyncReadCompatExt` for tokio compat |
| `async-io` | 2.x | `smol` | `Async<T>` fd wrapper |

All optional behind feature flags. Core `rdma-io` remains dependency-free (beyond existing deps).

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

1. **Should `AsyncCq` own the `CompletionChannel`?** If yes, simpler lifetime. If no, channel can be shared across multiple CQs (uncommon but valid).
2. **Thread-safety of `AsyncCq`**: Should it be `Sync`? CQ polling is thread-safe, but the notifier state may not be. Likely `Send` but not `Sync` — one task owns it.
3. **`ibv_get_cq_event` blocking behavior**: When the fd is non-blocking, does `ibv_get_cq_event` return `EAGAIN` or block? Must verify on siw. If it blocks, we need to call it only after `epoll` confirms readability.
4. **Multiple CQs on one channel**: Should we support it? The RDMA spec allows it but it complicates event routing (each `ibv_get_cq_event` returns which CQ fired).
5. **Cancellation safety**: If a future is dropped mid-`poll`, are RDMA resources in a consistent state? The drain-after-arm pattern is inherently cancellation-safe since we re-arm on every loop iteration.

---

## 14. Future Work

1. **Async timeouts** — Wrap verb/stream ops with `tokio::time::timeout()` or provide built-in `with_timeout(Duration)` combinators. Current design blocks forever on missing completions.
2. **Graceful async shutdown** — Design `poll_close` / `shutdown()` on `AsyncRdmaStream` that drains pending send/recv WRs before `rdma_disconnect`, instead of abrupt disconnect in Drop.
3. **Stream split** — `AsyncRdmaStream::into_split()` → `AsyncReadHalf` / `AsyncWriteHalf` for concurrent read+write from separate tasks. Requires shared `AsyncCq` (via `Arc`) and separate send/recv state.
4. **Error recovery & QP state transitions** — Document what happens when a verb fails mid-flight (QP transitions to error state). Distinguish fatal vs. recoverable errors. Design `reset_qp()` or reconnect flow.
5. **Completion routing / dispatch** — Current `poll_wr_id()` silently drops non-matching completions. Need a proper dispatch mechanism (e.g., `HashMap<u64, Waker>`) for multiplexing multiple in-flight operations on one CQ.
6. **Cancellation safety documentation** — Document precisely which async operations are safe to cancel (drop the future) and which may leak posted WRs. Posted RECV WRs that are never completed are leaked until QP destroy.
7. **Backpressure / flow control (async)** — Adapt the sync stream's credit-based flow control for async context. Sender must track remote RECV buffer availability; `write()` should return `Pending` when no credits available rather than posting to a full QP.
8. **`Send`/`Sync` bounds audit** — Explicitly verify and document `Send`/`Sync` bounds for `AsyncQp`, `AsyncRdmaStream`, `AsyncCq`. All must be `Send` to work with `tokio::spawn`. Add compile-time assertions: `fn assert_send<T: Send>() {}`.
9. **Shared Receive Queue (SRQ) async support** — `AsyncSrq` for connection-dense servers where thousands of QPs share a receive pool, reducing memory usage.
10. **Memory window (MW) support** — Fine-grained remote access control without re-registering MRs. Useful for zero-copy RPC frameworks.
11. **io_uring integration** — Use `io_uring` to submit `epoll_wait` or direct RDMA operations (if supported by future drivers), eliminating syscall overhead for the notification path.
