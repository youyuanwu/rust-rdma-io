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
│                     │     │  ConnectionLifetime (drop)    │
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
   - Phase C (Shutdown): QP transitions to error state (generating real flush CQEs), CQ drivers enter cooperative final drain barriers, and the driver exits only after the drain/cleanup path completes. Each CQ driver uses a real-time `tokio::time::Instant` deadline of 5 seconds instead of `DRAIN_BARRIER_BUDGET` (4096 iterations), yielding every 32 drain iterations so the runtime stays responsive. If the CQ futures are already empty, the Phase C drain loop exits immediately instead of stalling for 5 seconds. After CQ driver exit, inflight maps are closed unconditionally so no waiter can hang on already-stopped drivers.

2. **`Pin<Box<dyn Future>>` for driver**: Avoids exposing complex generic types. The boxed future erases the CQ driver type (Fd vs Polling) and completion mode generics.

3. **`TransportSharedState` with atomic state + Notify**: Provides a race-free lifecycle protocol between frontend and driver. The `Notify` is used with register-before-check to avoid lost wakeups. In particular, `send()`'s pool-wait path creates `notified()` first and only then checks `is_terminal()`, giving a register-check-recheck protocol that closes the M-1 lost-wakeup window during driver abort.

4. **`impl Drop for MessageTransportDriver`**: Synchronously marks the driver dead and wakes all frontend waiters. This handles the never-polled and aborted-mid-await cases that a boxed future body cannot cover.

5. **`compare_exchange` for state transitions**: Both `close()` (Created/Ready → Closing) and driver readiness (Created → Ready) use `compare_exchange` to detect concurrent transitions, preventing close/ready races.

6. **Connection ownership split**: `ConnectionLifetime` is a single Arc-shared owner of SharedQp, completion channels, Pd, CmId, and EventChannel. Both the frontend (`MessageTransport`) and the driver (`MessageTransportDriver`) hold `Arc<ConnectionLifetime>` via `TransportSharedState`. No standalone `Arc<SharedQp>` escapes this owner — all QP access is borrowed through `conn_lifetime.shared_qp()`. The struct's field declaration order guarantees safe destruction: SharedQp (QP → `rdma_destroy_qp`) drops before completion channels (`ibv_destroy_comp_channel`), then Pd, then CmId (`rdma_destroy_id`), then EventChannel. When the last holder drops, the destructor runs in this safe order regardless of which side was last.

7. **MR quarantine on shutdown/abort**: `OpFuture` returns `(Result<Completion>, Option<Mr>)`. On a real CQE, `Some(mr)` is returned — the hardware is confirmed done. On driver shutdown (inflight map closed), `None` is returned — the MR is pushed to the `CqDriverHandle` reclaim queue for safe destruction. Reclaim is now time-based: `RECLAIM_DEADLINE` (30 seconds) replaced `RECLAIM_MAX_TURNS`, so wedged-provider quarantine during normal driver operation no longer depends on scheduler cadence and does not falsely quarantine under normal scheduling. During shutdown, the CQ driver exits and `drain_reclaimed()` is not called again, so any remaining entries stay queued until `CqDriverHandle::drop`, which structurally follows QP destruction per `ConnectionLifetime` and `SharedQp` field ordering. This ensures the invariant: **an MR posted to hardware may be returned/reused/dropped only after its actual CQE is reaped OR the owning QP has been synchronously destroyed.**

8. **No synthetic completions**: Previous versions used `flush_all()` to write synthetic WrFlushErr completions during shutdown, which could return MRs to callers before the hardware was done with them. The current design replaces this with `InflightMap::close()` which wakes waiters to quarantine their MRs, and the CQ driver drain barrier processes real flush CQEs from QP→ERR transition. The internal shutdown helper is now `close_and_shutdown()`, reflecting that it closes the inflight map before signalling shutdown.

9. **Credit-frame handling is fail-safe**: `pending_credits` is no longer zeroed until `post_send_and_detach()` succeeds. If registry allocation or WR posting fails, `post_send_and_detach()` returns the control MR to the pool via `on_reclaim`, preventing silent pool shrink on control-path errors.

10. **Protocol and teardown failures are terminal**: Malformed CREDIT frames, general protocol parse failures, repost failures, CM/verbs failures, and CQ-driver failures all set `terminal_error` and terminate the transport. Phase C logs QP shutdown failure as a warning instead of silently ignoring it.

### Error Observation

Errors are observable in two places:
1. **Driver future `Result<()>`**: The `JoinHandle` result from `tokio::spawn(driver).await`
2. **Frontend `error()` method**: `transport.error()` returns an `Option<TransportError>` — a cloneable, thread-safe snapshot with typed category (`TransportErrorKind`) and human-readable message

Both channels observe consistent cause information from the same terminal event. The error is stored exactly once with race-safe compare-exchange semantics. Frontend operations (`ready()`, `send()`, `recv()`) return `Error::TransportFailed(TransportError)` when the driver has failed, preserving the typed cause rather than reducing to opaque `TransportClosed`. After CQ-driver shutdown, Phase C closes inflight maps unconditionally — not only on drain timeout — so all remaining waiters observe the same terminal outcome.

CQ-driver failures surface as `ConnectionError`, not `CompletionError`, because the driver commonly returns `Error::Verbs`/connection-level failures and `TransportError::from_error()` maps those to `TransportErrorKind::ConnectionError`.

| Condition | `error()` | `ready()`/`send()`/`recv()` | Driver `Result` |
|-----------|-----------|---------------------------|-----------------|
| Clean close | `None` | `TransportClosed` | `Ok(())` |
| HELLO timeout | `Some(ProtocolViolation)` | `TransportFailed(...)` | `Err(ProtocolViolation)` |
| Driver dropped | `Some(DriverAborted)` | `TransportFailed(...)` | N/A (never ran) |
| Driver aborted | `Some(DriverAborted)` | `TransportFailed(...)` | `JoinError(Cancelled)` |
| CQ driver error | `Some(ConnectionError)` | `TransportFailed(...)` | `Err(...)` |
| Peer disconnect | `None` | `TransportClosed` | `Ok(())` |

### Ownership Structure and Drop-Order Proof

The core safety invariant is: `rdma_destroy_qp(cm_id_raw)` must run while the `CmId` (owner of `cm_id_raw`) is still alive, because `CmQueuePair::drop()` dereferences the raw `cm_id` pointer.

**`ConnectionLifetime`** enforces this structurally:

```rust
pub(crate) struct ConnectionLifetime {
    shared_qp: SharedQp,                         // drops FIRST → rdma_destroy_qp
    _completion_channels: Vec<Arc<CompletionChannel>>, // drops second
    pd: Pd,                                      // drops third (Arc refcount decrement)
    cm_id: CmId,                                 // drops fourth → rdma_destroy_id
    event_channel: Arc<EventChannel>,            // drops last
}
```

**Nested invariant**: `SharedQp { qp, send_handle, recv_handle, pd }` — `qp: Arc<Qp>` is the **first** field and must remain so, since the Qp destructor (`CmQueuePair::drop → rdma_destroy_qp`) requires the CmId to be alive.

**Precondition**: `SharedQp.qp` must be the last `Arc<Qp>` reference when `ConnectionLifetime` drops. This is enforced by not exposing `SharedQp` or `Arc<Qp>` through any public accessor — `shared_qp()` and `driver_handles()` were removed from `MessageTransport`.

**Arc sharing**: Both `MessageTransport` and `MessageTransportDriver` hold `Arc<ConnectionLifetime>` via `TransportSharedState`. When the driver is dropped/aborted, its Arc refcount decreases; the frontend still holds its reference. When the frontend also drops, the refcount reaches zero and `ConnectionLifetime` destructs in field order: SharedQp → CompletionChannels → Pd → CmId → EventChannel.

**No escape paths**: The public `shared_qp()` and `driver_handles()` methods have been removed from `MessageTransport`. All internal QP access borrows from `ConnectionLifetime` — no standalone `Arc<SharedQp>` or `Arc<Qp>` can outlive the lifetime owner.

**OpFuture safety**: `OpFuture` in the `Inflight` state holds `Arc<CqDriverHandle>` and `Mr` but NOT `Arc<Qp>`. The `Arc<Qp>` is only held during the `Pending → Inflight` transition (first poll). When an OpFuture is cancelled, it pushes its resources to the driver's reclaim queue without retaining a Qp reference. Therefore, no `Arc<Qp>` can outlive the `ConnectionLifetime`.

**Completion-channel proof**: Dropping `SharedQp` first releases the last `Arc<CompletionQueue>` references owned through the QP/CQ graph. Only then do `_completion_channels` drop, so `ibv_destroy_comp_channel` runs after associated CQs are gone and no longer returns `EBUSY`. The previous completion-channel leak signature (about 90 `EBUSY` occurrences per test run) is eliminated by this field ordering.

**Drop test coverage**: Six deterministic tests verify all scenarios:
- `test_lifetime_unspawned_driver_dropped_frontend_remains` — driver dropped before spawn
- `test_lifetime_spawned_driver_aborted_frontend_remains` — driver abort mid-flight
- `test_lifetime_frontend_dropped_driver_remains` — frontend drops first
- `test_lifetime_inflight_send_recv_cancellation` — cancelled send/recv
- `test_lifetime_final_owner_drop_order` — final-owner drop smoke test
- `test_connection_lifetime_field_drop_order` (in `connection.rs`) — structural field-order proof including completion channels

### Integration Points

- `FdCqDriver::run_tokio()` and `PollingCqDriver::run()` are boxed and composed inside the driver — their public API is unchanged.
- `SharedQp` is wrapped in `Arc` for sharing between frontend and driver.
- `CqDriverHandle` is shared via `Arc` as before — `OpFuture`, `push_detached`, and completion routing are unchanged.
- `ConnectionLifetime` now retains completion-channel `Arc`s so completion-channel destruction is ordered after CQ destruction and before `CmId` teardown.
- Driver-drop cleanup uses `close_and_shutdown()`; the older `flush_and_shutdown` name is gone.
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
- `test_no_hidden_spawn_in_v2`: Recursive source regression over `rdma-io/src/v2/**/*.rs`; rejects multiple hidden-spawn patterns (`tokio::spawn`, `tokio::task::spawn`, `spawn_blocking`, `std::thread::spawn`, `Handle::current().spawn`)
- `test_drop_unspawned_driver_fails_frontend`: Proves `Drop` guard works
- `test_frontend_close_exits_driver`: Proves `close()` signal reaches driver
- `test_frontend_drop_exits_driver`: Proves frontend drop is detected
- `test_one_task_per_endpoint_shared_cq` / `_separate_cq`: Exercise real send/recv exchanges in shared-CQ and separate-CQ modes, proving the composed CQ-driver designs work end-to-end; the structural one-task guarantee is enforced by `test_no_hidden_spawn_in_v2`
- `test_readiness_mode_explicit_spawn` / `test_polling_mode_explicit_spawn`: Both completion modes
- `test_driver_abort_propagates_to_frontend`: Renamed driver-abort regression covering frontend error observation
- `test_hello_mismatch_fails_ready`: FR-027 regression proving HELLO capability mismatch fails `ready()` with a terminal protocol error
- `test_concurrent_send_abort_no_hang`: M-1 regression proving concurrent `send()` waiters do not lose wakeups when the driver aborts

### MR Teardown Safety Tests

- `test_qp_destroy_before_mr_deregistration_order`: Structural drop-recorder test proving QP destruction precedes CqDriverHandle (and thus reclaim-queue MR) deregistration
- `test_transport_shared_state_field_order`: Verifies `TransportSharedState` field drop order is safe (driver_handles before conn_lifetime)
- `test_inflight_map_close_wakes_waiters`: Unit test for `InflightMap::close()` mechanism
- `test_mr_quarantine_on_driver_abort`: Integration test — abort a spawned driver with connection established, verify frontend errors correctly and cleanup is safe
- `test_mr_quarantine_on_unspawned_driver_drop`: Integration test — drop unspawned driver, verify pre-posted recv MRs are quarantined
- `test_graceful_close_drains_real_cqes`: Integration test — graceful `close()` with an active connection, verifying close → Phase C → drain → cleanup; it does not intentionally leave an in-flight WR outstanding at the instant of close
- `test_connection_lifetime_field_drop_order`: Existing structural test for ConnectionLifetime field order, including completion-channel ordering

## Limitations and Future Work

- **Tokio-specific**: The driver future uses `tokio::select!`, `Notify`, `Semaphore`, and `AsyncFd`. It cannot be polled on non-Tokio runtimes.
- **Frontend not `Clone`**: Share via `Arc<MessageTransport>` if needed.
- **Wedged provider quarantine**: If a provider fails to generate flush CQEs after QP→ERR within `DRAIN_TIMEOUT`, the driver stops waiting rather than freeing MRs unsafely. After that shutdown point, `drain_reclaimed()` is no longer called; remaining reclaim entries simply stay queued until `ConnectionLifetime` teardown reaches `CqDriverHandle` drop after QP destruction. What remains lost is reusable pool capacity for that transport instance.
- **Shutdown closes registration**: Once an inflight map is closed during teardown, `InflightMap::register()` rejects new work. Late repost/send-control attempts therefore fail fast instead of re-opening shutdown paths.
- **Drain convergence depends on explicit future drops**: `hello_send` is dropped before the drain barrier so its inflight slot cannot keep Phase C alive indefinitely; any future reordering must preserve that property.
- **`OpFuture` output change**: `OpFuture::Output` is `(Result<Completion>, Option<Mr>)` rather than `(Result<Completion>, Mr)`. Callers must handle the `None` case (MR quarantined during shutdown). This is a breaking API change from the initial v2 design.
