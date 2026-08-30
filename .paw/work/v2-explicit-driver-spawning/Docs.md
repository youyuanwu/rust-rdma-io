# V2 Explicit Driver Spawning

## Overview

Refactors the v2 message transport API to eliminate all hidden `tokio::spawn` calls, returning a `(MessageTransport, MessageTransportDriver)` pair from `connect()`/`accept()`. The caller explicitly spawns the driver future on a Tokio runtime. This provides full visibility into task count, lifecycle ownership, and error observation.

Zero `tokio::spawn` calls exist in `rdma-io/src/v2/*.rs` production code. A source-level regression test enforces this.

## Architecture and Design

### High-Level Architecture

```
┌─────────────────────┐     ┌──────────────────────────────┐
│  MessageTransport   │     │  MessageTransportDriver       │
│  (frontend handle)  │     │  (spawned by caller)          │
│                     │     │                               │
│  send() ──────────┐ │     │  ┌─ CQ Driver(s) ──────────┐ │
│  recv() ──────────┤ │◄───►│  │  FdCqDriver/Polling      │ │
│  ready() ─────────┤ │     │  └──────────────────────────┘ │
│  close() ─────────┤ │     │  ┌─ Recv Pump ──────────────┐ │
│                   │ │     │  │  frame parsing, credits   │ │
│  TransportShared  │ │     │  └──────────────────────────┘ │
│  State (Arc)  ◄───┘ │     │  ┌─ Disconnect Monitor ─────┐ │
│                     │     │  │  CM event watching        │ │
│                     │     │  └──────────────────────────┘ │
│                     │     │  ┌─ HELLO Handshake ─────────┐ │
│                     │     │  │  credit initialization    │ │
│                     │     │  └──────────────────────────┘ │
│                     │     │  ConnectionResources (drop)   │
└─────────────────────┘     └──────────────────────────────┘
```

### Task Count

- **Shared CQ mode** (default): 1 user-spawned task per endpoint
- **Separate CQ mode**: 1 user-spawned task per endpoint (both CQ drivers composed inside the driver future via `FuturesUnordered`)
- `send()`/`recv()` run in the caller's task context, not the driver's

### Design Decisions

1. **Phased driver state machine**: The driver runs three sequential phases:
   - Phase A (Handshake): CQ drivers run concurrently with HELLO send/receive. CQ drivers MUST be polled from the first poll because HELLO send/recv produce `OpFuture`s that require CQ dispatch.
   - Phase B (Steady state): CQ drivers, recv pump, and disconnect monitor run concurrently in a `select!` loop.
   - Phase C (Shutdown): QP transitions to error, CQ drivers drain their final barriers with a bounded timeout.

2. **`Pin<Box<dyn Future>>` for driver**: Avoids exposing complex generic types. The boxed future erases the CQ driver type (Fd vs Polling) and completion mode generics.

3. **`TransportSharedState` with atomic state + Notify**: Provides a race-free lifecycle protocol between frontend and driver. The `Notify` is used with register-before-check to avoid lost wakeups (similar pattern to `OpFuture`'s completion waiting).

4. **`impl Drop for MessageTransportDriver`**: Synchronously marks the driver dead and wakes all frontend waiters. This handles the never-polled and aborted-mid-await cases that a boxed future body cannot cover.

5. **`compare_exchange` for state transitions**: Both `close()` (Created/Ready → Closing) and driver readiness (Created → Ready) use `compare_exchange` to detect concurrent transitions, preventing close/ready races.

6. **Connection ownership split**: `ConnectionLifetime` is a single Arc-shared owner of SharedQp, Pd, CmId, and EventChannel. Both the frontend (`MessageTransport`) and the driver (`MessageTransportDriver`) hold `Arc<ConnectionLifetime>` via `TransportSharedState`. No standalone `Arc<SharedQp>` escapes this owner — all QP access is borrowed through `conn_lifetime.shared_qp()`. The struct's field declaration order guarantees safe destruction: SharedQp (QP → `rdma_destroy_qp`) drops before CmId (`rdma_destroy_id`) drops before EventChannel. When the last holder drops, the destructor runs in this safe order regardless of which side was last.

### Error Observation

Errors are observable in two places:
1. **Driver future `Result<()>`**: The `JoinHandle` result from `tokio::spawn(driver).await`
2. **Frontend `error()` method**: `transport.error()` returns an `Option<TransportError>` — a cloneable, thread-safe snapshot with typed category (`TransportErrorKind`) and human-readable message

Both channels observe consistent cause information from the same terminal event. The error is stored exactly once with race-safe compare-exchange semantics. Frontend operations (`ready()`, `send()`, `recv()`) return `Error::TransportFailed(TransportError)` when the driver has failed, preserving the typed cause rather than reducing to opaque `TransportClosed`.

| Condition | `error()` | `ready()`/`send()`/`recv()` | Driver `Result` |
|-----------|-----------|---------------------------|-----------------|
| Clean close | `None` | `TransportClosed` | `Ok(())` |
| HELLO timeout | `Some(ProtocolViolation)` | `TransportFailed(...)` | `Err(ProtocolViolation)` |
| Driver dropped | `Some(DriverAborted)` | `TransportFailed(...)` | N/A (never ran) |
| Driver aborted | `Some(DriverAborted)` | `TransportFailed(...)` | `JoinError(Cancelled)` |
| CQ driver error | `Some(CompletionError)` | `TransportFailed(...)` | `Err(...)` |
| Peer disconnect | `None` | `TransportClosed` | `Ok(())` |

### Ownership Structure and Drop-Order Proof

The core safety invariant is: `rdma_destroy_qp(cm_id_raw)` must run while the `CmId` (owner of `cm_id_raw`) is still alive, because `CmQueuePair::drop()` dereferences the raw `cm_id` pointer.

**`ConnectionLifetime`** enforces this structurally:

```rust
pub(crate) struct ConnectionLifetime {
    shared_qp: SharedQp,              // drops FIRST → rdma_destroy_qp
    pd: Pd,                           // drops second (Arc refcount decrement)
    cm_id: CmId,                      // drops THIRD → rdma_destroy_id
    event_channel: Arc<EventChannel>, // drops last
}
```

**Nested invariant**: `SharedQp { qp, send_handle, recv_handle, pd }` — `qp: Arc<Qp>` is the **first** field and must remain so, since the Qp destructor (`CmQueuePair::drop → rdma_destroy_qp`) requires the CmId to be alive.

**Precondition**: `SharedQp.qp` must be the last `Arc<Qp>` reference when `ConnectionLifetime` drops. This is enforced by not exposing `SharedQp` or `Arc<Qp>` through any public accessor — `shared_qp()` and `driver_handles()` were removed from `MessageTransport`.

**Arc sharing**: Both `MessageTransport` and `MessageTransportDriver` hold `Arc<ConnectionLifetime>` via `TransportSharedState`. When the driver is dropped/aborted, its Arc refcount decreases; the frontend still holds its reference. When the frontend also drops, the refcount reaches zero and `ConnectionLifetime` destructs in field order: QP before CmId.

**No escape paths**: The public `shared_qp()` and `driver_handles()` methods have been removed from `MessageTransport`. All internal QP access borrows from `ConnectionLifetime` — no standalone `Arc<SharedQp>` or `Arc<Qp>` can outlive the lifetime owner.

**OpFuture safety**: `OpFuture` in the `Inflight` state holds `Arc<CqDriverHandle>` and `Mr` but NOT `Arc<Qp>`. The `Arc<Qp>` is only held during the `Pending → Inflight` transition (first poll). When an OpFuture is cancelled, it pushes its resources to the driver's reclaim queue without retaining a Qp reference. Therefore, no `Arc<Qp>` can outlive the `ConnectionLifetime`.

**Drop test coverage**: Five deterministic tests verify all scenarios:
- `test_lifetime_unspawned_driver_dropped_frontend_remains` — driver dropped before spawn
- `test_lifetime_spawned_driver_aborted_frontend_remains` — driver abort mid-flight
- `test_lifetime_frontend_dropped_driver_remains` — frontend drops first
- `test_lifetime_inflight_send_recv_cancellation` — cancelled send/recv
- `test_lifetime_final_owner_drop_order` — structural field-order proof

### Integration Points

- `FdCqDriver::run_tokio()` and `PollingCqDriver::run()` are boxed and composed inside the driver — their public API is unchanged.
- `SharedQp` is wrapped in `Arc` for sharing between frontend and driver.
- `CqDriverHandle` is shared via `Arc` as before — `OpFuture`, `push_detached`, and completion routing are unchanged.
- V1 APIs are completely unaffected.

## User Guide

### Basic Usage

```rust
// Build and connect — returns (frontend, driver) pair
let (transport, driver) = MessageTransportBuilder::new()
    .completion_mode(CompletionMode::Readiness)
    .connect(addr)
    .await?;

// Spawn the driver (exactly one task)
let driver_task = tokio::spawn(driver);

// Wait for readiness, then communicate
transport.ready().await?;
transport.send(b"hello").await?;
let msg = transport.recv().await?;

// Shutdown
transport.close().await;
let result = driver_task.await.expect("panicked");
result?; // observe driver errors
```

### Failing to Spawn the Driver

If the driver is dropped without being spawned/polled:
- `ready()`, `send()`, `recv()` return `Error::TransportFailed(TransportError)` with kind `DriverAborted`
- `close()` returns immediately (driver already dead)
- `error()` returns `Some(TransportError)` with kind `DriverAborted`
- No resources leak — `Drop for MessageTransportDriver` handles cleanup

### Recommended Shutdown Order

1. `transport.close().await` — signals driver to shut down
2. `driver_task.await` — observe driver completion and errors
3. `transport.error()` — inspect terminal error if driver failed
4. Drop the transport (if not already dropped)

### Lifecycle Matrix

| Frontend action | Driver state | Result | `error()` |
|----------------|-------------|--------|-----------|
| `ready()` | Not spawned | Hangs until driver spawned or dropped | `None` |
| `ready()` | Spawned, running | Completes after HELLO | `None` |
| `ready()` | Dropped/failed | `TransportFailed(DriverAborted)` | `Some(DriverAborted)` |
| `send()` | Not ready | Waits for readiness internally | `None` |
| `send()` | Ready | Normal send | `None` |
| `send()` | Stopped | `TransportClosed` | `None` |
| `send()` | Failed | `TransportFailed(...)` | `Some(...)` |
| `close()` | Running | Signals shutdown, waits for terminal state | `None` |
| `close()` | Already stopped | Returns immediately | `None` |
| Drop frontend | Driver running | Driver detects and shuts down | — |
| Drop driver | Frontend waiting | Frontend fails with `TransportFailed` | `Some(DriverAborted)` |
| Abort driver task | Frontend waiting | `Drop` guard fires, frontend fails | `Some(DriverAborted)` |

## Testing

### How to Test

```bash
# No-hidden-spawn regression (no RDMA hardware needed)
cargo test --test v2_no_hidden_spawn

# All v2 message transport tests (requires RXE or SIW)
cargo test --test v2_message_transport_tests

# Full workspace
cargo test --workspace
```

### Key Test Scenarios

- `test_no_progress_without_driver_poll`: Proves driver must be polled for readiness
- `test_drop_unspawned_driver_fails_frontend`: Proves `Drop` guard works
- `test_frontend_close_exits_driver`: Proves `close()` signal reaches driver
- `test_frontend_drop_exits_driver`: Proves frontend drop is detected
- `test_one_task_per_endpoint_shared_cq` / `_separate_cq`: Structural task count assertions
- `test_readiness_mode_explicit_spawn` / `test_polling_mode_explicit_spawn`: Both completion modes

## Limitations and Future Work

- **Tokio-specific**: The driver future uses `tokio::select!`, `Notify`, `Semaphore`, and `AsyncFd`. It cannot be polled on non-Tokio runtimes.
- **Frontend not `Clone`**: Share via `Arc<MessageTransport>` if needed.
- **Deprecated escape hatches**: `shared_qp()` and `driver_handles()` are deprecated because standalone `Arc<SharedQp>` access can outlive the connection lifetime, risking use-after-free. Use `send()`/`recv()`/`close()` instead.
