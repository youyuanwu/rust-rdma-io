# Rsocket Rust Bindings Design

**Status:** Design  
**Date:** 2026-03-14

## Goal

Create safe Rust bindings and an async stream for the rsocket API (`librdmacm`), preparing for future InfiniBand/RoCE hardware where rsocket's RDMA Write-based byte stream is fully functional. On current software providers (siw/rxe), the data path is broken, but connection management and partial APIs work — enough to build and test the binding layer.

## API Surface

rsocket exposes 26 functions in `/usr/include/rdma/rsocket.h`. Grouped by layer:

### Connection Management (works on siw + rxe)

| C Function | Rust Wrapper | Notes |
|------------|-------------|-------|
| `rsocket()` | `RSocket::new()` | Returns fd-like handle |
| `rbind()` | `RSocket::bind()` | |
| `rlisten()` | `RSocket::listen()` | |
| `raccept()` | `RSocket::accept() → RSocket` | |
| `rconnect()` | `RSocket::connect()` | |
| `rshutdown()` | `RSocket::shutdown()` | |
| `rclose()` | `Drop` impl | |

### Byte Stream — rsend/rrecv (broken on siw + rxe, works on hardware)

| C Function | Rust Wrapper | Notes |
|------------|-------------|-------|
| `rsend()` | `RSocket::send()` | RDMA Write + Immediate Data |
| `rrecv()` | `RSocket::recv()` | |
| `rread()` | `RSocket::read()` | Convenience (send with flags=0) |
| `rwrite()` | `RSocket::write()` | Convenience (recv with flags=0) |
| `rreadv()` | `RSocket::readv()` | Vectored I/O |
| `rwritev()` | `RSocket::writev()` | Vectored I/O |
| `rsendto()` | — | UDP-style, skip for now |
| `rrecvfrom()` | — | UDP-style, skip for now |
| `rsendmsg()` | — | Advanced, skip for now |
| `rrecvmsg()` | — | Advanced, skip for now |

### Remote I/O (64B works on siw + rxe, larger sizes hang)

| C Function | Rust Wrapper | Notes |
|------------|-------------|-------|
| `riomap()` | `RSocket::iomap()` | Map local buffer for remote access |
| `riounmap()` | `RSocket::iounmap()` | |
| `riowrite()` | `RSocket::iowrite()` | Write to remote mapped region |

> Note: There is no `rioread()`. The remote side reads from its locally mapped buffer where data was placed by `riowrite()`.

### Polling & Options (expected to work on siw + rxe)

| C Function | Rust Wrapper | Notes |
|------------|-------------|-------|
| `rpoll()` | `rpoll()` (free fn) | Mirrors `poll(2)` |
| `rselect()` | — | Skip, rpoll is sufficient |
| `rsetsockopt()` | `RSocket::set_opt()` | `SOL_RDMA` options |
| `rgetsockopt()` | `RSocket::get_opt()` | |
| `rfcntl()` | `RSocket::fcntl()` | Nonblocking mode |
| `rgetsockname()` | `RSocket::local_addr()` | |
| `rgetpeername()` | `RSocket::peer_addr()` | |

### SOL_RDMA Socket Options

```c
#define SOL_RDMA 0x10000
enum { RDMA_SQSIZE, RDMA_RQSIZE, RDMA_INLINE, RDMA_IOMAPSIZE, RDMA_ROUTE };
```

| Option | Type | Description |
|--------|------|-------------|
| `RDMA_SQSIZE` | `u32` | Send queue depth |
| `RDMA_RQSIZE` | `u32` | Recv queue depth |
| `RDMA_INLINE` | `u32` | Inline data size |
| `RDMA_IOMAPSIZE` | `u32` | Number of remote I/O mappings |
| `RDMA_ROUTE` | `struct ibv_path_data` | RDMA route info |

## Architecture

### Layer 1: FFI Bindings (rdma-io-sys)

Add rsocket as a new partition in `bnd-rdma-gen/rdma.toml`:

```toml
[[partition]]
namespace = "rdma.rsocket"
library = "rdmacm"          # rsocket lives in librdmacm
headers = ["rdma/rsocket.h"]
traverse = ["rdma/rsocket.h"]
```

This generates raw FFI in `rdma-io-sys/src/rdma/rsocket/mod.rs` — consistent with the existing `ibverbs` and `rdmacm` partitions.

### Layer 2: Safe Sync Wrapper (rdma-io)

```rust
// rdma-io/src/rsocket.rs

/// Owns an rsocket file descriptor. Closed on Drop.
pub struct RSocket {
    fd: i32,
}

impl RSocket {
    pub fn new(domain: Domain, ty: Type) -> Result<Self>;
    pub fn bind(&self, addr: &SocketAddr) -> Result<()>;
    pub fn listen(&self, backlog: i32) -> Result<()>;
    pub fn accept(&self) -> Result<(RSocket, SocketAddr)>;
    pub fn connect(&self, addr: &SocketAddr) -> Result<()>;
    pub fn shutdown(&self, how: Shutdown) -> Result<()>;

    pub fn send(&self, buf: &[u8], flags: i32) -> Result<usize>;
    pub fn recv(&self, buf: &mut [u8], flags: i32) -> Result<usize>;
    pub fn read(&self, buf: &mut [u8]) -> Result<usize>;
    pub fn write(&self, buf: &[u8]) -> Result<usize>;

    pub fn set_nonblocking(&self, nonblock: bool) -> Result<()>;
    pub fn local_addr(&self) -> Result<SocketAddr>;
    pub fn peer_addr(&self) -> Result<SocketAddr>;

    pub fn set_opt<T>(&self, opt: RdmaOpt, val: &T) -> Result<()>;
    pub fn get_opt<T>(&self, opt: RdmaOpt) -> Result<T>;

    pub fn iomap(&self, buf: &mut [u8], prot: i32, flags: i32) -> Result<off_t>;
    pub fn iounmap(&self, buf: &mut [u8]) -> Result<()>;
    pub fn iowrite(&self, buf: &[u8], offset: off_t, flags: i32) -> Result<usize>;
}

impl Drop for RSocket {
    fn drop(&mut self) { unsafe { rclose(self.fd); } }
}

// fd is a plain integer — Send + Sync are safe
unsafe impl Send for RSocket {}
unsafe impl Sync for RSocket {}
```

### Layer 3: Async Stream (rdma-io)

Since rsocket fds are user-space (not kernel fds), `tokio::AsyncFd` cannot be used. Instead, async integration requires an `RpollReactor` thread (see "Critical Design Constraint" section below).

```rust
// rdma-io/src/async_rsocket.rs

/// Async RDMA stream using rsocket, for use with tokio.
pub struct AsyncRSocket {
    inner: RSocket,
    reactor: Arc<RpollReactor>,
    read_ready: Arc<AtomicBool>,
    write_ready: Arc<AtomicBool>,
}

impl AsyncRSocket {
    /// Connect to a remote address.
    pub async fn connect(addr: &SocketAddr) -> Result<Self>;

    /// Get a raw reference for socket options.
    pub fn as_rsocket(&self) -> &RSocket;
}

/// Async listener using rsocket.
pub struct AsyncRSocketListener {
    inner: RSocket,
    reactor: Arc<RpollReactor>,
    accept_ready: Arc<AtomicBool>,
}

impl AsyncRSocketListener {
    pub async fn bind(addr: &SocketAddr) -> Result<Self>;
    pub async fn accept(&self) -> Result<(AsyncRSocket, SocketAddr)>;
    pub fn local_addr(&self) -> Result<SocketAddr>;
}
```

**Tokio integration approach:**
1. Create rsocket fd with `rsocket()`
2. Set nonblocking via `rfcntl(fd, F_SETFL, O_NONBLOCK)` (requires C wrapper for variadic)
3. Register fd with a shared `RpollReactor` thread that calls `rpoll()` in a loop
4. Implement `AsyncRead`/`AsyncWrite` — reactor wakes tokio wakers when fd is ready, then Rust code calls `rsend()`/`rrecv()`

This is more complex than `AsyncRdmaStream` (which uses `tokio::AsyncFd` on real kernel fds from completion channels). The rpoll thread is the price of rsocket's user-space fd model.

### Tonic Integration (rdma-io-tonic)

Once `AsyncRSocket` implements `AsyncRead + AsyncWrite + Send + Sync + Unpin`, it plugs into `tonic-tls` the same way `TokioRdmaStream` does today — potentially with a `TokioRSocketStream` newtype or by making the tonic transport generic over the stream type.

## What Can Be Tested on Software Providers

### Fully Testable (siw + rxe)

| Area | Test | Why It Works |
|------|------|-------------|
| **Connection lifecycle** | connect → accept → close | CM path is functional |
| **Listener** | bind → listen → accept multiple clients | CM path |
| **Address queries** | `local_addr()`, `peer_addr()` after connect | Socket metadata, no data path |
| **Socket options** | set/get `RDMA_SQSIZE`, `RDMA_RQSIZE`, `RDMA_INLINE` | Configuration, no data path |
| **Nonblocking mode** | `set_nonblocking()` + verify `WouldBlock` on empty recv | Control plane |
| **rpoll** | Poll for readable/writable events after connect | Event notification |
| **rpoll integration** | `AsyncRSocket::connect()` + `AsyncRSocketListener::accept()` | Async connection management |
| **Drop/close** | Verify clean shutdown, no fd leaks | Resource lifecycle |
| **Error handling** | Connect to unreachable addr, bind to used port | Error paths |
| **Concurrent connections** | Multiple clients connecting to one listener | Stress test CM path |

### Partially Testable

| Area | Test | Limitation |
|------|------|-----------|
| **riowrite 64B** | Map buffer → iowrite 64 bytes → verify | ≥4KB hangs on both providers |
| **riomap/riounmap** | Map → unmap lifecycle | Can verify mapping succeeds, not data flow at scale |

### Not Testable (requires hardware)

| Area | Why |
|------|-----|
| **rsend/rrecv** | "Connection reset by peer" on both siw and rxe |
| **AsyncRead/AsyncWrite data path** | Depends on rsend/rrecv |
| **riowrite ≥4KB** | Hangs on both providers |
| **Throughput benchmarks** | No working data path |
| **Tonic gRPC over rsocket** | Needs working byte stream |

### Test Strategy

```
tests/
  rsocket_tests.rs
    ├── test_connect_accept()           // ✅ siw + rxe
    ├── test_listener_multiple()        // ✅ siw + rxe
    ├── test_local_peer_addr()          // ✅ siw + rxe
    ├── test_socket_options()           // ✅ siw + rxe
    ├── test_nonblocking_wouldblock()   // ✅ siw + rxe
    ├── test_rpoll_events()             // ✅ siw + rxe
    ├── test_async_connect_accept()     // ✅ siw + rxe
    ├── test_connect_refused()          // ✅ siw + rxe
    ├── test_drop_no_leak()             // ✅ siw + rxe
    ├── test_riowrite_small()           // ⚠️ 64B only
    ├── test_rsend_rrecv()              // ❌ expect failure, verify graceful error
    └── test_send_recv_echo()           // ❌ hardware-only, gated by feature flag
```

**CI approach:** Run connection management tests in CI (siw + rxe). Gate data-path tests behind a `#[cfg(feature = "rsocket-hw")]` feature flag that's only enabled on hardware CI runners.

## Implementation Phases

### Phase 1: FFI Bindings
- Add `rsocket` partition to `bnd-rdma-gen/rdma.toml`
- Regenerate bindings with `cargo run -p bnd-rdma-gen`
- Verify rsocket symbols appear in `rdma-io-sys`

### Phase 2: Safe Sync Wrapper
- Implement `RSocket` struct with connection management
- Implement socket options, nonblocking, address queries
- Add rsend/rrecv/riowrite (even though untestable on SW providers)
- Write connection lifecycle tests (siw + rxe)

### Phase 3: Async Wrapper
- Implement `RpollReactor` thread (shared across connections)
- Implement `AsyncRSocket` and `AsyncRSocketListener`
- Implement `AsyncRead`/`AsyncWrite` for `AsyncRSocket`
- Write async connection tests

### Phase 4: Tonic Integration (hardware-dependent)
- Generic transport layer or `TokioRSocketStream` newtype
- TLS support via `tonic-tls`
- End-to-end gRPC tests (hardware-only)

## Critical Design Constraint: rsocket fd Internals

### rsocket fds ARE real kernel fds — but you can't epoll them for data

Contrary to some documentation, rsocket fds **are** real kernel file descriptors. The `rsocket()` function returns the CM event channel fd:

```c
// From rdma-core librdmacm/rsocket.c
int rsocket(int domain, int type, int protocol) {
    ...
    if (type == SOCK_STREAM) {
        rdma_create_id(NULL, &rs->cm_id, rs, RDMA_PS_TCP);
        index = rs->cm_id->channel->fd;   // ← real kernel fd!
    } else {
        ds_init(rs, domain);
        index = rs->udp_sock;             // ← real kernel fd!
    }
    rs_insert(rs, index);  // stores rsocket struct at idm[index]
    return rs->index;      // returns the same integer
}
```

However, **data readiness lives on a different internal fd**. Inside `rpoll()`, the `rs_poll_arm` function maps rsocket fds to the correct kernel fd based on state:

```c
// rpoll internally remaps to the right fd:
if (rs->state >= rs_connected)
    rfds[i].fd = rs->cm_id->recv_cq_channel->fd;  // CQ completion channel
else
    rfds[i].fd = rs->cm_id->channel->fd;           // CM event channel
```

Additionally, `rpoll` does CQ busy-polling, credit processing, and state management before falling through to `poll()`.

### Why tokio::AsyncFd won't work

| Approach | Works? | Why |
|----------|--------|-----|
| `epoll(rsocket_fd)` for connect | ⚠️ Partially | rsocket fd = CM event channel fd, gets CM events |
| `epoll(rsocket_fd)` for data | ❌ | Data readiness is on `recv_cq_channel->fd` (internal, different fd) |
| `tokio::io::unix::AsyncFd` | ❌ | Would miss data events and all rpoll internal processing |
| `libc::fcntl(rsocket_fd, O_NONBLOCK)` | ⚠️ | Sets CM channel nonblocking, but `rfcntl` also sets CQ channel |
| `rfcntl(fd, F_SETFL, O_NONBLOCK)` | ✅ | Sets both CM and CQ channels nonblocking (via `rs_set_nonblocking`) |
| `rpoll(&fds, nfds, timeout)` | ✅ | Full state-aware polling with CQ busy-poll + correct fd mapping |

**You must use `rfcntl()` for nonblocking and `rpoll()` for event readiness.** rpoll's internal logic (CQ busy-polling, state-dependent fd mapping, credit updates) cannot be replicated externally.

### Async Strategy: rpoll Reactor Thread with eventfd Wakeup

Since `tokio::AsyncFd` won't work, the async layer needs a dedicated rpoll thread. The critical insight is that **rpoll already avoids busy-looping** internally:

1. **Busy-poll phase** (~10µs): Polls CQ directly for low-latency fast path
2. **Block phase**: Arms CQ for notification, then calls real `poll()` on kernel CQ channel fds — blocks until event

Additionally, **rpoll passes non-rsocket fds through to regular `poll()`**. This lets us include an `eventfd` in the rpoll set as a wakeup mechanism — no busy loop, no timeout-based polling.

```
┌──────────────────────┐     ┌─────────────────────────────────────┐
│  tokio runtime        │     │  rpoll reactor thread                │
│                      │     │                                     │
│  AsyncRSocket         │     │  pollfds = [rsock1, rsock2, wakefd] │
│   .poll_read(cx) ────┼──►  │                                     │
│     register waker   │     │  loop {                             │
│     return Pending   │     │    rpoll(pollfds, n, -1) ◄── blocks │
│                      │     │    // rpoll internally:             │
│   ◄──────────────────┼──   │    //   busy-poll CQ 10µs          │
│   waker.wake()       │     │    //   poll(cq_channel_fds, -1)   │
│   .poll_read(cx)     │     │    wake registered wakers           │
│     rrecv(fd, buf)   │     │  }                                  │
│     return Ready(n)  │     │                                     │
└──────────────────────┘     └─────────────────────────────────────┘
         ▲                            │
         └──── waker.wake() ──────────┘
```

**Key: eventfd for registration wakeup**

When a new rsocket is registered (or deregistered), we need to wake the rpoll thread to rebuild its pollfd set. Since rpoll passes unknown fds to regular `poll()`:

```rust
struct RpollReactor {
    thread: JoinHandle<()>,
    state: Arc<ReactorState>,
}

struct ReactorState {
    // eventfd for waking the rpoll thread on registration changes
    wake_fd: RawFd,  // created via libc::eventfd(0, EFD_NONBLOCK)

    // Protected by mutex — only accessed briefly to add/remove registrations
    registrations: Mutex<Vec<Registration>>,
}

struct Registration {
    rsocket_fd: i32,
    interest: Interest,       // Read | Write | Both
    waker: Waker,
}
```

**Reactor thread loop (no busy loop):**

```rust
fn reactor_loop(state: Arc<ReactorState>) {
    loop {
        // 1. Snapshot registrations + build pollfd array
        let regs = state.registrations.lock().clone();
        let mut pollfds: Vec<pollfd> = regs.iter().map(|r| pollfd {
            fd: r.rsocket_fd,
            events: r.interest.to_poll_events(),
            revents: 0,
        }).collect();

        // 2. Append the wake eventfd — rpoll passes non-rsocket fds to poll()
        pollfds.push(pollfd {
            fd: state.wake_fd,
            events: POLLIN,
            revents: 0,
        });

        // 3. Block in rpoll — NO busy loop
        //    rpoll internally: busy-poll CQ 10µs, then poll(kernel_fds, -1)
        let ret = unsafe { rpoll(pollfds.as_mut_ptr(), pollfds.len(), -1) };

        // 4. Check if woken by eventfd (registration change)
        if let Some(wake_pfd) = pollfds.last() {
            if wake_pfd.revents & POLLIN != 0 {
                // Drain the eventfd
                let mut buf = [0u8; 8];
                libc::read(state.wake_fd, buf.as_mut_ptr().cast(), 8);
                continue;  // Rebuild pollfds with new registrations
            }
        }

        // 5. Wake tokio tasks whose rsocket fds are ready
        for (i, reg) in regs.iter().enumerate() {
            if pollfds[i].revents != 0 {
                reg.waker.wake_by_ref();
            }
        }
    }
}
```

**Registration from AsyncRead/AsyncWrite:**

```rust
impl tokio::io::AsyncRead for AsyncRSocket {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>)
        -> Poll<io::Result<()>>
    {
        // Try non-blocking recv first
        match rrecv(self.fd, buf.initialize_unfilled(), MSG_DONTWAIT) {
            Ok(n) => { buf.advance(n); return Poll::Ready(Ok(())); }
            Err(e) if e.would_block() => { /* fall through to register */ }
            Err(e) => return Poll::Ready(Err(e)),
        }

        // Register waker with rpoll reactor
        self.reactor.register(self.fd, Interest::Read, cx.waker().clone());

        // Try once more (avoid race between register and event)
        match rrecv(self.fd, buf.initialize_unfilled(), MSG_DONTWAIT) {
            Ok(n) => { buf.advance(n); Poll::Ready(Ok(())) }
            Err(e) if e.would_block() => Poll::Pending,
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}
```

**Why this doesn't busy-loop:**
- `rpoll(fds, n, -1)` with timeout=-1 blocks indefinitely in kernel `poll()` after the 10µs CQ busy-poll phase
- The eventfd wakes the thread only when registrations change (rare — once per new connection)
- Between events, the thread sleeps in kernel space with zero CPU usage

**Latency characteristics:**
- Best case: ~10µs (CQ busy-poll catches it during the fast path)
- Typical: One kernel wakeup latency (~1-5µs on modern Linux)
- No 100ms polling timeout — purely event-driven

**One thread serves all connections** — the pollfds array contains all registered rsocket fds. This is equivalent to how epoll serves all fds in a single `epoll_wait`.

**Trade-off vs AsyncRdmaStream:**

| Aspect | AsyncRdmaStream | AsyncRSocket (rpoll reactor) |
|--------|----------------|------------------------------|
| Event notification | tokio AsyncFd on CQ channel fd | rpoll thread on CQ channel fd |
| Thread overhead | Zero (epoll-integrated) | One thread per reactor |
| Wake latency | One epoll edge trigger | 10µs busy-poll + one poll wake |
| Buffer management | Manual (our code) | Automatic (rsocket internal) |
| CQ processing | Our code | rsocket internal |

## Other Open Questions

1. **bnd-rdma-gen compatibility:** Does `bnd-winmd` handle rsocket.h correctly? The header uses `off_t`, `struct msghdr`, variadic `rfcntl()` — may need wrapper functions in `rdma-io-sys/wrapper/` for unsupported patterns.

2. **rfcntl variadic:** C variadic functions can't be called directly from Rust. Add wrappers to the existing `rdma-io-sys/wrapper/wrapper.{h,c}` (same pattern used for inline ibverbs functions):
   ```c
   // wrapper.h
   int rdma_wrap_rfcntl_getfl(int socket);
   int rdma_wrap_rfcntl_setfl(int socket, int flags);

   // wrapper.c
   #include <rdma/rsocket.h>
   int rdma_wrap_rfcntl_getfl(int socket) { return rfcntl(socket, F_GETFL); }
   int rdma_wrap_rfcntl_setfl(int socket, int flags) { return rfcntl(socket, F_SETFL, flags); }
   ```
   These get compiled into `librdma_wrapper` and picked up by the existing `wrapper` partition in `bnd-rdma-gen/rdma.toml`.

3. **Performance vs AsyncRdmaStream:** On hardware, rsocket manages its own QP/CQ internally. Our AsyncRdmaStream has fine-grained control over buffer sizes and CQ polling. The rpoll thread adds latency that raw verbs don't have. Is rsocket's convenience worth the overhead for gRPC workloads?

## References

- [rsocket.h](file:///usr/include/rdma/rsocket.h) — Full API
- [rsocket(7) man page](https://manpages.debian.org/testing/librdmacm-dev/rsocket.7.en.html)
- [rdma-core rsocket.c](https://github.com/linux-rdma/rdma-core/blob/master/librdmacm/rsocket.c) — Implementation
- [Rsocket.md](Rsocket.md) — Transport compatibility testing results
