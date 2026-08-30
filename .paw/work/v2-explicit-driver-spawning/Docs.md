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
2. **Frontend state**: `ready()`/`send()`/`recv()` return `Error::TransportClosed` when the driver has failed or stopped

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
3. Drop the transport (if not already dropped)

### Lifecycle Matrix

| Frontend action | Driver state | Result |
|----------------|-------------|--------|
| `ready()` | Not spawned | Hangs until driver spawned or dropped |
| `ready()` | Spawned, running | Completes after HELLO |
| `ready()` | Dropped/failed | `Error::TransportClosed` |
| `send()` | Not ready | Waits for readiness internally |
| `send()` | Ready | Normal send |
| `send()` | Stopped/failed | `Error::TransportClosed` |
| `close()` | Running | Signals shutdown, waits for terminal state |
| `close()` | Already stopped | Returns immediately |
| Drop frontend | Driver running | Driver detects and shuts down |
| Drop driver | Frontend waiting | Frontend operations fail |
| Abort driver task | Frontend waiting | `Drop` guard fires, frontend fails |

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
- **`error()` API**: A `transport.error()` method for inspecting driver errors was planned but deferred. Errors are currently observed via state transitions and the driver's `Result<()>`.
- **`TransportError` type**: A cloneable error wrapper for dual-channel error observation was planned but deferred.
- **Frontend not `Clone`**: Share via `Arc<MessageTransport>` if needed.
