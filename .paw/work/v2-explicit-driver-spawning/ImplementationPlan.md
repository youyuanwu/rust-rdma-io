# V2 Explicit Driver Spawning Implementation Plan

## Overview

Refactor the v2 message transport API to eliminate all hidden `tokio::spawn` calls and return a two-object `(MessageTransport, MessageTransportDriver)` pair from `connect()`/`accept()`. The driver future composes all background work — CQ completion driving, receive pumping, HELLO/credit protocol, disconnect monitoring, and cancellation reclamation — into a single `Future<Output = Result<()>> + Send + 'static` that the caller explicitly spawns.

## Current State Analysis

**Hidden spawns (4 sites)**:
- `connection.rs:411-416`: `spawn_driver()` spawns CQ driver tasks via `tokio::spawn`
- `message_transport.rs:568`: `tokio::spawn(recv_pump(...))`
- `message_transport.rs:589`: `tokio::spawn(disconnect_monitor(...))`

**Current flow**: `MessageTransportBuilder::connect/accept` → `ConnectionBuilder::connect/accept` (spawns CQ drivers) → `MessageTransport::from_connection` (performs HELLO handshake synchronously, then spawns recv_pump + disconnect_monitor) → returns `MessageTransport`

**Driver architecture**: `FdCqDriver::run()`/`PollingCqDriver::run()` consume `self` into infinite async loops. They already return `Result<()>` and implement proper shutdown via `CqDriverHandle::shutdown()`.

**Key constraint**: `Connection` currently owns `Vec<JoinHandle<Result<()>>>` for driver tasks, and its `Drop` aborts them. This tightly couples spawning to connection setup.

## Desired End State

- `connect()`/`accept()` return `Result<(MessageTransport, MessageTransportDriver)>`
- `MessageTransportDriver` implements `Future<Output = Result<()>> + Send + 'static`
- Zero `tokio::spawn` in `rdma-io/src/v2/*.rs` production code
- HELLO handshake occurs inside the driver future; `MessageTransport` exposes `ready().await`
- `send()`/`recv()` internally await readiness before proceeding
- All existing tests pass with updated call sites + new lifecycle tests added
- Exactly one user-spawned task per endpoint (shared or separate CQ)

## What We're NOT Doing

- Changing V1 APIs
- Implementing a custom executor/reactor/event loop
- Multi-connection driver sharing
- Async runtime abstraction layer (Tokio remains required; the driver is spawnable by the caller on Tokio but not runtime-neutral)
- Changing the public `FdCqDriver`/`PollingCqDriver` `(driver, handle)` API contract
- Adding new transport features beyond explicit spawning
- Making `MessageTransport` implement `Clone` (use `Arc<MessageTransport>` for sharing)

## Phase Status

- [ ] **Phase 1: Core Driver Composition** — Restructure Connection to not spawn; create MessageTransportDriver composing CQ driver + recv pump + disconnect monitor + HELLO into one future
- [ ] **Phase 2: Frontend Readiness & Lifecycle** — Add ready()/send()/recv() readiness gating, close() without JoinHandle, Drop semantics for both objects, shared state transitions
- [ ] **Phase 3: Tests & Regression Guard** — Update all test call sites, add lifecycle/determinism tests, add no-hidden-spawn regression check
- [ ] **Phase 4: Provider Validation** — Full RXE + SIW workspace validation with provider switching
- [ ] **Phase 5: Documentation** — Update README, rustdoc, create Docs.md

## Phase Candidates
<!-- No candidates at this time -->

---

## Phase 1: Core Driver Composition

### Changes Required:

- **`rdma-io/src/v2/connection.rs`**: 
  - Replace `spawn_driver()` with `create_driver()` that returns boxed driver futures without spawning
  - Split `Connection` into: `ConnectionResources` (owns Pd, CmId, EventChannel, AsyncFd in safe drop order) for the driver, and shared references (Arc<SharedQp>, Arc<CqDriverHandle>) for the frontend
  - Remove `driver_tasks: Vec<JoinHandle<...>>` field entirely
  - Remove the public `Connection` type from transport API surface; replace `MessageTransport::connection()` accessor with narrower `shared_qp()` and `driver_handles()` methods
  - Update `initiate_shutdown()` and `Drop` to work without JoinHandles
  - Update both `connect()` and `accept()` paths for shared and separate CQ modes

- **`rdma-io/src/v2/message_transport.rs`**:
  - Change `MessageTransportBuilder::connect/accept` return type from `Result<MessageTransport>` to `Result<(MessageTransport, MessageTransportDriver)>`
  - Split `from_connection()`: resource setup + channel/pool allocation stays in connect/accept; HELLO + recv_pump + disconnect_monitor move into the driver future
  - Remove `recv_pump_task`/`disconnect_task` JoinHandle fields from `MessageTransport`
  - Add `TransportSharedState` struct with lifecycle state machine, Notify, credits, error, frontend_alive flag
  - Create `MessageTransportDriver` struct with `Pin<Box<dyn Future<...> + Send>>`, `Arc<TransportSharedState>`, and `impl Drop` that synchronously marks driver dead and wakes all waiters
  - Driver future uses phased execution: Phase A (HELLO concurrent with CQ drivers) → Phase B (steady-state select loop) → Phase C (shutdown drain). CQ drivers are pinned and polled from the very first poll, never dropped by select!
  - Move `recv_pump()` and `disconnect_monitor()` into the driver future's steady-state loop

- **`rdma-io/src/v2/mod.rs`**: Add `MessageTransportDriver` to the tokio-gated re-exports

- **`rdma-io-tests/tests/v2_message_transport_tests.rs`**: Update `make_transport_pair()` helper to destructure `(transport, driver)` tuple and `tokio::spawn(driver)`, keeping all 29 existing tests compiling against the new API

### Phase Dependencies

Phases are strictly linear: 1 → 2 → 3 → 4 → 5. Each phase depends on the prior. No parallelization — lifecycle semantics (Phase 2) require the structural changes in Phase 1, tests (Phase 3) require the API to be complete, etc.

### Architecture: Driver Future Composition

The `MessageTransportDriver` wraps a boxed future and implements `Drop` for cleanup:

```rust
pub struct MessageTransportDriver {
    inner: Pin<Box<dyn Future<Output = Result<()>> + Send>>,
    state: Arc<TransportSharedState>, // drop guard
}

impl Drop for MessageTransportDriver {
    fn drop(&mut self) {
        // Synchronous cleanup: mark driver dead, close credits,
        // wake all waiters — runs even if never polled or aborted
        self.state.mark_driver_dead();
    }
}
```

The inner future uses a phased state machine, NOT a one-shot `select!`:

**Phase A — Handshake**: CQ driver(s) run concurrently with HELLO via `select!` in a loop. HELLO sends our frame, awaits peer HELLO (which requires CQ driver to dispatch completions), validates, initializes credits, sets ready flag. The CQ driver branches are pinned and remain alive across phases.

**Phase B — Steady state**: CQ driver(s), recv_pump, and disconnect_monitor all run in a `loop { select! { ... } }`. When any branch signals terminal state (disconnect, error, close request), the loop breaks.

**Phase C — Shutdown**: Transition QP to error, flush_and_shutdown driver handles, continue polling CQ driver(s) through their final drain barriers, then exit.

Critical: CQ drivers are polled from the very first poll of the composed future (concurrent with HELLO). They are never dropped by `select!` — they live as pinned futures across all phases.

### Ownership Split: Connection → Frontend + Driver Resources

**Design decision** (resolves Connection ownership):
- `Connection` is split at construction time. `MessageTransport` (frontend) receives:
  - `Arc<SharedQp>` — for QP access in send/recv operations
  - `Arc<CqDriverHandle>` refs — for inflight map registration / work notification
  - Channels (send pool, recv msg, repost)
  - `Arc<TransportSharedState>` — shared lifecycle state
- `MessageTransportDriver` (driver) receives:
  - `ConnectionResources` struct (owns `Pd`, `CmId`, `EventChannel`, `AsyncFd`) preserving safe drop order: QP Arc released → Pd → CmId → EventChannel
  - CQ driver futures (boxed)
  - `Arc<CqDriverHandle>` refs (for shutdown)
  - `Arc<SharedQp>` — for HELLO send and recv_pump operations
  - `Arc<TransportSharedState>` — shared lifecycle state
  - Pre-posted recv OpFuture vec and channel endpoints for recv_pump
- The public `MessageTransport::connection()` accessor is removed (breaking change). Replace with narrower accessors: `shared_qp()`, `driver_handles()` if needed for tests.

### Shared Lifecycle State (TransportSharedState)

Replace individual Arc<AtomicBool> fields with a unified struct:

```rust
struct TransportSharedState {
    state: AtomicU8,        // Created=0, Running=1, Ready=2, Closing=3, Stopped=4, Failed=5
    state_notify: Notify,   // wakes ready(), send(), recv(), close() on state changes
    remote_credits: Semaphore, // initialized empty, filled by driver after HELLO
    error: Mutex<Option<Arc<TransportError>>>, // cloneable error snapshot
    frontend_alive: AtomicBool, // set false in Drop for MessageTransport
}
```

**Readiness waiting protocol** (avoids lost-wakeup races):
1. Create `state_notify.notified()` future (registers interest)
2. Check `state` — if Ready/Stopped/Failed, return immediately
3. Await the notified future
4. Goto 1

**Frontend-drop detection**: `frontend_alive: AtomicBool` is set to `false` in `Drop for MessageTransport`. The driver checks this each iteration and in its select branches. A `Notify` wakes the driver when frontend drops.

**Error model**: `TransportError` is a new cloneable error type (or `Arc<Error>` wrapper) that can be stored once and read from both the driver's `Result<()>` and `transport.error()`. Components (recv_pump, disconnect_monitor) return typed exit reasons rather than silent returns.

**`close(&self)` semantics**: Sets state to `Closing`, closes credits, notifies driver. Then waits on `state_notify` for terminal state (Stopped/Failed). Returns immediately if state is already terminal (covers unspawned driver case via Drop guard).

### Success Criteria (Phase 1):

#### Automated Verification:
- [ ] `cargo check --workspace` compiles without errors
- [ ] `cargo build --features tokio` compiles without errors
- [ ] `grep -rn 'tokio::spawn' rdma-io/src/v2/ | grep -vE '^\s*(///|//!)' | grep -v '#\[doc'` returns zero results (canonical exclusion: `///` and `//!` doc-comment lines)
- [ ] Completion notification recovery pattern preserved in composed driver
- [ ] Single CQ/sole poller ownership and generation routing unchanged

#### Manual Verification:
- [ ] `MessageTransportBuilder::connect()` returns `(MessageTransport, MessageTransportDriver)`
- [ ] `MessageTransportDriver` is accepted by `tokio::spawn()` (type check)
- [ ] `Connection` no longer stores `JoinHandle`s
- [ ] CQ drivers are composed inside the driver future, not spawned separately

---

## Phase 2: Frontend Readiness & Lifecycle

### Changes Required:

- **`rdma-io/src/v2/message_transport.rs`**:
  - Add `ready(&self) -> impl Future<Output = Result<()>>`: uses register-check-recheck protocol with `state_notify` to avoid lost wakeups; returns `TransportClosed` if state is Failed/Stopped
  - Add `error(&self) -> Option<Arc<TransportError>>`: frontend error inspection API
  - Modify `send()`: await readiness internally before credit acquisition; check state for terminal conditions
  - Modify `recv()`: await readiness internally before channel recv; check state for terminal conditions
  - Implement `close(&self) -> impl Future<Output = ()>`: set state to Closing, close credits, notify driver via `state_notify`, then wait on terminal state (Stopped/Failed); returns immediately if state already terminal (handles unspawned/dead driver)
  - Update `Drop for MessageTransport`: set `frontend_alive = false`, notify driver, close credits (no JoinHandle abort)
  - `Drop for MessageTransportDriver` (Phase 1 structure): sets state=Failed, closes credits, wakes `state_notify` — runs even if never polled or aborted mid-await

- **`rdma-io/src/v2/message_transport.rs` (driver internals)**:
  - Driver Phase A (HELLO): concurrent with CQ drivers, send HELLO, await peer HELLO, validate, add credits to shared Semaphore, transition state to Ready, notify waiters
  - Driver Phase B (steady-state): recv_pump + disconnect_monitor + CQ driver(s) in `loop { select! { } }`. Each component returns a typed exit reason rather than silent return
  - Driver Phase C (shutdown): on close/frontend-drop/error/disconnect, transition QP to error, `flush_and_shutdown` driver handles, continue polling CQ drivers through their final drain barriers (do NOT drop CQ futures before drain completes), then transition state to Stopped and exit
  - Error propagation: store `Arc<TransportError>` in shared state, set state=Failed, the same error is returned from the driver's `Result<()>`

- **`rdma-io/src/v2/connection.rs`**:
  - `Connection::close()` simplified to `initiate_shutdown()` (no JoinHandles to await)
  - Remove public `Connection` from transport API; keep as internal `ConnectionResources`

- **`rdma-io/src/v2/error.rs`**: Reuse `TransportClosed`/`DriverShutdown` for driver-not-polled scenarios (no new variant needed); add `TransportError` cloneable wrapper type for dual-channel error observation

### HELLO Error Timing Change (FR-027):

After this change, `connect()`/`accept()` no longer fail on HELLO timeout or protocol mismatch — those errors surface through the driver's `Result<()>` and `ready().await`. The `# Errors` rustdoc sections of `connect`/`accept` must be updated in Phase 5.

### Success Criteria:

#### Automated Verification:
- [ ] `cargo build --features tokio` compiles
- [ ] `cargo clippy --features tokio -- -D warnings` passes
- [ ] Basic smoke: a simple client/server test with explicit spawn works

#### Manual Verification:
- [ ] `ready().await` blocks until HELLO completes
- [ ] `send()`/`recv()` internally await readiness
- [ ] `close().await` signals driver without needing JoinHandle
- [ ] Dropping unspawned driver sets `driver_alive = false` and wakes waiters
- [ ] Frontend drop while driver runs triggers driver shutdown

---

## Phase 3: Tests & Regression Guard

### Changes Required:

- **`rdma-io-tests/tests/v2_message_transport_tests.rs`**:
  - `make_transport_pair()` already updated in Phase 1; all 29 existing tests compile and pass
  - Add new lifecycle tests:
    - `test_no_progress_without_driver_poll`: construct pair, do NOT spawn driver, verify send/recv do not complete within bounded timeout
    - `test_readiness_completes_after_both_drivers`: spawn both drivers, verify `ready().await` completes
    - `test_drop_unspawned_driver_fails_frontend`: construct pair, drop driver immediately, verify `ready()`/`send()`/`recv()` return error
    - `test_abort_driver_task_fails_frontend`: spawn driver, abort the JoinHandle, verify frontend operations return error
    - `test_driver_error_propagates`: provoke a driver error, verify it's observable via `transport.error()` and driver future output
    - `test_frontend_close_exits_driver`: spawn driver, call `close().await`, verify driver future completes
    - `test_frontend_drop_exits_driver`: spawn driver, drop frontend, verify driver future completes (no orphan task)
    - `test_close_unspawned_driver_no_hang`: construct pair, drop driver, call `close().await`, verify it returns immediately
    - `test_one_task_per_endpoint_shared_cq`: structural assertion — verify only one JoinHandle per endpoint after spawn, plus SC-001 source check
    - `test_one_task_per_endpoint_separate_cq`: same with separate CQs
    - `test_readiness_mode_explicit_spawn`: full exchange with readiness mode
    - `test_polling_mode_explicit_spawn`: full exchange with polling mode
  - Update non-helper call sites affected by ownership changes: `test_recv_wakes_on_driver_shutdown` (uses `connection().driver_handles()`), tests calling `close()` (receiver type changed)

- **`rdma-io-tests/tests/v2_no_hidden_spawn.rs`** (new file):
  - Source-level regression test: read all `.rs` files under `rdma-io/src/v2/`, scan for `tokio::spawn` occurrences not inside doc-comment lines (`///` or `//!`), assert zero matches. Canonical exclusion rule: lines matching `^\s*(///|//!)` are skipped
  - This test runs without RDMA hardware (pure source analysis)

- **`rdma-io-tests/tests/v2_tests.rs`**: Review for any indirect v2 message transport usage; update if needed
- **`rdma-io-tests/tests/v2_shared_qp_tests.rs`**: Review; these use lower-level APIs that should remain unchanged

### Success Criteria:

#### Automated Verification:
- [ ] All 29 existing message transport tests pass: `cargo test --test v2_message_transport_tests`
- [ ] All new lifecycle tests pass
- [ ] No-hidden-spawn regression test passes: `cargo test --test v2_no_hidden_spawn`
- [ ] `cargo test --workspace` passes

#### Manual Verification:
- [ ] No test uses wall-clock sleeps for synchronization (channels, barriers, bounded timeouts only)
- [ ] Test coverage includes both readiness and polling completion modes
- [ ] Test coverage includes both shared and separate CQ modes

---

## Phase 4: Provider Validation

### Changes Required:

- **RXE validation** (provider should already be active):
  - Verify RXE is active: `rdma link show`
  - `cargo fmt --check`
  - `cargo build --all-targets`
  - `cargo clippy --workspace --features tokio -- -D warnings`
  - `cargo test --workspace`
  - `cargo test --doc --workspace --features tokio`
  - `cargo doc --no-deps`

- **SIW validation**:
  - Switch to SIW: `sudo ./scripts/setup-siw.sh`
  - `cargo test --workspace`
  - `cargo test --doc --workspace --features tokio`
  - Fix any provider-specific hangs with bounded timeouts

- **Restore RXE**:
  - `sudo ./scripts/setup-rxe.sh`
  - Verify RXE active: `rdma link show`
  - Re-run `cargo test --workspace` to confirm

- **Feature build matrix** (provider-independent, run once):
  - `cargo build --no-default-features`
  - `cargo build --features async`
  - `cargo build --features tokio`

### Success Criteria:

#### Automated Verification:
- [ ] All commands above pass on RXE
- [ ] All commands above pass on SIW
- [ ] RXE restored and confirmed active after SIW testing

#### Manual Verification:
- [ ] No provider-specific test hangs
- [ ] Zero clippy warnings with denied warnings

---

## Phase 5: Documentation

### Changes Required:

- **`.paw/work/v2-explicit-driver-spawning/Docs.md`**: Technical reference (load `paw-docs-guidance`)
  - API changes: old vs new `connect()`/`accept()` signatures
  - Architecture: driver composition diagram/description
  - Task count: one per endpoint (both CQ modes)
  - Lifecycle matrix: all frontend/driver state combinations
  - Error observation model: driver Result + frontend state
  - Shutdown order recommendation

- **`README.md`**:
  - Update v2 message transport example (lines ~157-205) to show `let (transport, driver) = ...` + `tokio::spawn(driver)`
  - Add lifecycle notes about driver spawning, readiness, shutdown

- **Rustdoc updates**:
  - `message_transport.rs` module doc (`:1-63`): update example to two-object pattern; update "dedicated CM event monitor task" prose to reflect driver-internal composition; update receive-buffer invariant to reference driver context
  - `MessageTransportBuilder` doc: update connect/accept return type docs and `# Errors` sections (HELLO failures now surface via driver/ready(), not connect/accept)
  - `MessageTransportDriver` doc: new type documentation with examples, lifecycle, Drop semantics
  - `MessageTransport` doc: update to reflect no background tasks, readiness requirement, `close(&self)` semantics
  - `ready()`, `close()`, `error()` method docs
  - `connection.rs:78-94`: remove/update `Connection` "Drop Order" documentation referencing deleted `driver_tasks` field
  - `driver.rs:224-235`, `:386-397`: verify `FdCqDriver`/`PollingCqDriver` rustdoc examples remain accurate for standalone usage

- **Doc tests**: Ensure all rustdoc examples compile (`cargo test --doc`)

### Success Criteria:

#### Automated Verification:
- [ ] `cargo test --doc` passes
- [ ] `cargo doc --no-deps` generates without warnings

#### Manual Verification:
- [ ] README example shows explicit spawn pattern
- [ ] Docs.md covers all required sections
- [ ] Task count documented for both CQ modes
- [ ] Lifecycle/error observation/shutdown order documented

---

## References

- Issue: none
- Spec: .paw/work/v2-explicit-driver-spawning/Spec.md
- Research: .paw/work/v2-explicit-driver-spawning/CodeResearch.md
