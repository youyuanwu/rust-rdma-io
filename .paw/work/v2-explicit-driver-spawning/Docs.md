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

6. **Connection ownership split**: `ConnectionParts` splits resources between frontend (`Arc<SharedQp>`, channels, shared state) and driver (`ConnectionResources` with Pd/CmId/EventChannel for safe drop ordering). The public `connection()` accessor is replaced by `shared_qp()` and `driver_handles()`.

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
- `ready()`, `send()`, `recv()` immediately return `Error::TransportClosed`
- `close()` returns immediately (driver already dead)
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
