# Feature Specification: V2 Explicit Driver Spawning

**Branch**: feature/v2-explicit-driver-spawning  |  **Created**: 2026-08-30  |  **Status**: Draft
**Input Brief**: Move v2 APIs to explicit user-controlled driver spawning with a two-object contract

## Overview

The v2 message transport API currently spawns background Tokio tasks internally during connection setup, hiding concurrency from the caller. This design forces callers onto Tokio, prevents integration with alternative runtimes, and makes lifecycle management (shutdown, error propagation, cancellation) implicit and harder to reason about.

This feature restructures the v2 message transport construction to return two objects: a **frontend handle** for application-level send/recv/close operations, and a **driver future** that owns all background processing. The caller explicitly spawns (or polls) the driver, giving full visibility into task count, lifecycle ownership, and error observation. The driver composes all internal activities — CQ completion driving, receive pumping, HELLO/credit protocol, disconnect monitoring, cancellation reclamation, and shutdown drain — into a single `Future<Output = Result<...>> + Send + 'static`, eliminating all hidden `tokio::spawn` calls from v2 production code.

This explicit-spawn model aligns with Rust ecosystem conventions (similar to `tonic`, `hyper`, `quinn`) where infrastructure futures are returned for caller-controlled spawning. It reduces the minimum task count to exactly one user-spawned task per endpoint while preserving full message transport functionality including credit flow control, buffer reuse, and graceful shutdown.

The change targets only v2 message transport APIs. V1 APIs remain entirely unchanged, and existing low-level v2 driver APIs that already follow the explicit `(driver, handle)` pattern are preserved and leveraged.

## Objectives

- **Eliminate hidden concurrency**: No `tokio::spawn` calls in v2 production code paths; every concurrent task is visible to and controlled by the caller
- **Minimize task overhead**: Exactly one user-spawned driver future per endpoint in both shared and separate CQ modes
- **Enable explicit caller-controlled spawning**: The driver future is an opaque `Future<Output = Result<()>> + Send + 'static` spawnable by the caller on a Tokio runtime; runtime abstraction is out of scope
- **Clarify lifecycle ownership**: Frontend and driver have well-defined ownership boundaries with deterministic behavior for all drop/cancel/error scenarios
- **Preserve transport capabilities**: Credit-based flow control, buffer reuse, HELLO validation, graceful shutdown, and completion notification recovery all continue to function correctly
- **Maintain safety guarantees**: No self-referential structures, no unsafe lifetime extension; safe Rust composition of internal futures

## User Scenarios & Testing

### User Story P1 – Explicit Driver Spawning for Message Transport

Narrative: A developer builds a message-passing application using v2 APIs. They construct a transport via `MessageTransportBuilder`, receiving a frontend handle and a driver future. They spawn the driver on their chosen runtime, wait for readiness, and proceed with send/recv operations. They observe exactly one spawned task per endpoint and retain full control over its lifecycle.

Independent Test: Construct a transport pair, spawn drivers, complete a send/recv exchange, and verify no additional tasks were spawned beyond the two explicitly created by the caller.

Acceptance Scenarios:
1. Given a `MessageTransportBuilder` configured for readiness completion mode, When `connect()` is awaited, Then a `(transport, driver)` pair is returned without any background tasks having been spawned
2. Given a returned driver future, When the user spawns it with `tokio::spawn(driver)`, Then transport readiness completes and send/recv operations succeed
3. Given a shared CQ configuration, When a connection is established, Then exactly one user-spawned task drives all CQ completions, receive pumping, HELLO protocol, disconnect monitoring, and cancellation reclamation
4. Given a separate CQ configuration, When a connection is established, Then still exactly one user-spawned driver future composes both CQ drivers internally without additional spawning

### User Story P2 – Deterministic Lifecycle Control

Narrative: A developer needs to understand and control exactly what happens during startup, shutdown, errors, and cancellation. The two-object contract provides clear ownership semantics: the frontend is for requests, the driver owns resources and progress.

Independent Test: Drop an unspawned driver and verify that all frontend operations immediately return errors without hanging.

Acceptance Scenarios:
1. Given a transport pair where the driver has not been spawned/polled, When `transport.send()` is called, Then no progress occurs (send blocks or returns NotReady) because the driver is not running
2. Given a running driver task, When `transport.close().await` is called, Then the driver performs graceful shutdown (flush/reclaim in-flight MRs) and transitions to completed state
3. Given a running driver task, When the driver task is aborted, Then all pending frontend operations (ready/send/recv/credit waits) wake with an error
4. Given a running driver task, When the peer disconnects, Then the disconnect is detected by the driver and propagated to all frontend waiters
5. Given a transport where the frontend is dropped while the driver runs, Then the driver detects frontend absence and shuts down gracefully
6. Given a driver that returns an error, When the user awaits the spawn handle, Then the error is observable both via the driver's `Result` output and via frontend state inspection

### User Story P3 – Updated Documentation and Examples

Narrative: A developer reads the README and rustdoc to learn the v2 message transport API. All examples show the explicit two-object spawn pattern with clear lifecycle guidance.

Independent Test: All rustdoc examples compile and pass (`cargo test --doc`).

Acceptance Scenarios:
1. Given the README v2 message transport section, When a developer reads it, Then the example shows `let (transport, driver) = ...` with explicit `tokio::spawn(driver)` and lifecycle notes
2. Given the rustdoc for `MessageTransportBuilder`, When a developer reads it, Then the documentation explains task count (one per endpoint), readiness requirements, error observation, and shutdown order

### User Story P4 – No-Hidden-Spawn Regression Protection

Narrative: A CI maintainer wants assurance that future changes cannot reintroduce hidden spawning in v2 production code.

Independent Test: A source-level check confirms no `tokio::spawn` calls exist in `rdma-io/src/v2/` outside of documentation examples.

Acceptance Scenarios:
1. Given the v2 production source files, When a regression check scans for `tokio::spawn`, Then zero occurrences are found outside of `#[doc]` / `///` example blocks
2. Given a CI pipeline, When the regression check runs, Then it fails the build if a hidden spawn is introduced

### Edge Cases

- **Driver dropped before spawn**: Frontend transitions to closed/failed state; all waiters wake with error
- **Driver aborted mid-operation**: In-flight operations complete or cancel; MRs are reclaimed; frontend observes failure
- **Frontend dropped while driver runs**: Driver detects this and initiates shutdown; no orphan tasks remain
- **Both peers' drivers running but HELLO not yet exchanged**: Transport is not ready; `send()`/`recv()` internally await readiness
- **Credit exhaustion during shutdown**: Outstanding credits are reclaimed during driver shutdown drain
- **Separate CQ mode**: Both send and recv CQ drivers composed within the single driver future, not spawned separately

## Requirements

### Functional Requirements

- FR-001: `MessageTransportBuilder::connect()` and `accept()` return a `(MessageTransport, MessageTransportDriver)` pair without spawning any background tasks (Stories: P1)
- FR-002: `MessageTransportDriver` implements `Future<Output = Result<()>> + Send + 'static`, directly passable to `tokio::spawn()` or equivalent (Stories: P1)
- FR-003: The driver future internally composes all background activities: CQ completion driving, receive completion pumping, HELLO/credit protocol, CM disconnect monitoring, cancellation reclamation, and shutdown drain (Stories: P1)
- FR-004: In shared CQ mode, exactly one user-spawned task per endpoint suffices for full transport operation (Stories: P1)
- FR-005: In separate CQ mode, the single driver future composes both CQ drivers without additional spawning (Stories: P1)
- FR-006: `transport.ready().await` completes only after both peers' drivers are running and HELLO handshake with credit installation succeeds (Stories: P1, P2)
- FR-007: `send()` and `recv()` internally await readiness before proceeding, providing ergonomic usage without requiring explicit ready() calls (Stories: P1)
- FR-008: All `tokio::spawn` calls are removed from v2 production code; spawn is allowed only in tests and documentation examples (Stories: P1, P4)
- FR-009: `ConnectionBuilder` construction stops internally spawning CQ drivers; returns unspawned driver components for MessageTransport to compose (Stories: P1)
- FR-010: Dropping an unspawned driver transitions frontend state to closed/failed and wakes all waiters (Stories: P2)
- FR-011: Aborting a spawned driver task wakes all pending frontend operations with error (Stories: P2)
- FR-012: `transport.close().await` signals shutdown to the driver and waits for state transition without owning the `JoinHandle` (Stories: P2)
- FR-013: Driver shutdown ensures all in-flight MRs are safely handled: either (a) real CQEs are reaped during the CQ drain barrier and MRs are returned/dropped only after their CQE, or (b) MRs are quarantined in the reclaim queue and freed only after QP destruction. No synthetic completion may transfer MR ownership back to callers. (Stories: P2)
- FR-014: Peer disconnect is detected by the driver and propagated to all frontend waiters deterministically (Stories: P2)
- FR-015: Frontend drop while driver runs causes driver to detect absence and shut down gracefully with no orphan tasks (Stories: P2)
- FR-016: Driver errors are observable both via the driver future's `Result` output and via frontend state/error inspection (Stories: P2)
- FR-017: CQ driver primitives (`FdCqDriver`, `PollingCqDriver`) provide composable step/state-machine interfaces or safely composable pinned futures (Stories: P1)
- FR-018: Completion notification recovery pattern (work notification + bounded fallback CQ polling while work is in flight) is preserved (Stories: P1)
- FR-019: Single CQ/sole poller ownership and generation-protected completion routing are preserved (Stories: P1)
- FR-020: `OpFuture::Drop` remains centralized with no per-operation spawning (Stories: P1)
- FR-021: V1 APIs remain entirely unchanged (Stories: P1)
- FR-022: README and all v2 rustdoc updated to show explicit spawn pattern with lifecycle guidance (Stories: P3)
- FR-023: Documentation states exact task count, readiness requirements, error observation model, and recommended shutdown order (Stories: P3)
- FR-024: A source-level regression test or CI check prevents reintroduction of hidden spawning in v2 production code (Stories: P4)
- FR-025: Both readiness and polling completion modes work with the new explicit spawn API (Stories: P1)
- FR-026: Receive and control buffers are pre-posted before transport becomes ready; transport readiness requires HELLO validation and remote credit installation (Stories: P1)
- FR-027: HELLO validation and timeout failures are reported through the driver result and `ready()`, not from `connect()`/`accept()` (Stories: P1, P2)
- FR-028: Teardown safety invariant: an MR posted to hardware may be returned/reused/dropped only after its actual CQE is reaped OR the owning QP has been synchronously destroyed. `OpFuture` returns `Option<Mr>` — `Some(mr)` on real CQE, `None` when quarantined. `InflightMap::close()` wakes waiters who quarantine MRs via `push_detached`. MRs in the reclaim queue are freed only when `CqDriverHandle` drops, which structurally follows QP destruction per `ConnectionLifetime` field ordering. (Stories: P2)
- FR-029: On driver abort/drop, the inflight map is closed synchronously, waking all waiters with `DriverShutdown` errors. Waiters quarantine their MRs (return `None` to callers). No task remains to drain CQEs, but QP destruction at `ConnectionLifetime` drop time guarantees hardware is done before MRs are freed. (Stories: P2)
- FR-030: On wedged provider (RECLAIM_DEADLINE exceeded), registry slots are released but MRs are quarantined (kept alive in the reclaim queue). Resources are leaked rather than freed unsafely. (Stories: P2)
- FR-031: Credit balance invariant: the driver persists the negotiated peer receive capacity from HELLO and validates every CREDIT frame before adding permits. Zero-credit frames are rejected. Credits that would raise `available_permits` above the negotiated capacity are rejected as `ProtocolViolation`. The primary invariant tracks `credits_in_flight` (incremented after `permit.forget()` in the synchronous section of `send()`, decremented on valid CREDIT receipt), which is immune to the acquire→forget TOCTOU: temporarily-acquired but not-yet-forgotten permits cannot inflate the allowance. A belt-and-suspenders capacity check with overflow-safe arithmetic provides defense-in-depth. The peer's `data_recv_capacity` is validated during HELLO (must be > 0 and ≤ `Semaphore::MAX_PERMITS`). A violating CREDIT terminates the driver through normal failure/shutdown and exposes the typed cause through the driver result and `error()`. (Stories: P1, P2)

### Key Entities

- **MessageTransport**: Frontend handle for send/recv/close operations; cheap/cloneable if practical; owns only shared handles/channels/state needed to submit and receive
- **MessageTransportDriver**: Single composed driver future owning all background processing, connection/QP/CM/CQ resources; implements `Future<Output = Result<()>> + Send + 'static`
- **CqDriverHandle**: Shared handle for completion routing between driver and operation futures (existing, preserved)
- **MessageTransportBuilder**: Builder for constructing transport pairs with configurable completion mode (existing, return type changes)

### Cross-Cutting / Non-Functional

- No self-referential structures or unsafe lifetime extension in driver composition
- Boxed internal futures acceptable if type-safe with clear ownership
- No wall-clock sleeps in tests; use channels, barriers, manual polling, bounded timeouts as hang guards only
- Provider validation on both RXE and SIW soft-RDMA providers

## Success Criteria

- SC-001: Zero `tokio::spawn` calls in `rdma-io/src/v2/*.rs` production code outside documentation example blocks (FR-008, FR-024)
- SC-002: `connect()` and `accept()` return `(MessageTransport, MessageTransportDriver)` pair (FR-001)
- SC-003: `MessageTransportDriver` accepted by `tokio::spawn()` without wrapper (FR-002)
- SC-004: Complete send/recv exchange works with exactly one spawned driver per endpoint (FR-003, FR-004)
- SC-005: Separate CQ mode works with one spawned driver per endpoint (FR-005)
- SC-006: Transport readiness completes only after both drivers run and HELLO succeeds (FR-006, FR-026)
- SC-007: Unspawned driver drop wakes/fails all frontend operations (FR-010)
- SC-008: Aborted driver task wakes/fails all frontend operations (FR-011)
- SC-009: `close().await` triggers graceful shutdown without owning JoinHandle (FR-012, FR-013)
- SC-010: Peer disconnect propagates to frontend (FR-014)
- SC-011: All existing tests pass with updated v2 call sites (FR-021)
- SC-012: `cargo clippy`, `cargo test`, `cargo doc --no-deps` all pass cleanly (FR-022, FR-025)
- SC-013: Full workspace validation passes on both RXE and SIW providers (FR-025)
- SC-014: Regression check is automated and blocks builds on violation (FR-024)

## Assumptions

- The existing `(driver, handle)` pattern in `FdCqDriver`/`PollingCqDriver` can be adapted to return composable futures rather than requiring method calls that consume `self` into infinite loops
- A boxed future (`Pin<Box<dyn Future<Output = Result<()>> + Send>>`) is acceptable for the composed driver to avoid exposing complex generic types
- The HELLO handshake can be performed inside the driver future after it starts running, with readiness signaled through shared state
- Frontend cloneability can be achieved through `Arc`-wrapped shared state (consistent with existing `Arc<AtomicBool>` pattern for `closed` flag)
- RXE and SIW providers are both available and functional on the test host
- The recv pump and disconnect monitor loops can be composed alongside CQ driving within a single `select!`-based future using only safe Rust and `Arc`-shared state, as they do not hold cross-await borrows on non-`Send` types

## Scope

In Scope:
- Refactoring `MessageTransportBuilder::connect/accept` return types to two-object pair
- Composing all background work into single driver future
- Refactoring `ConnectionBuilder` to not internally spawn CQ drivers
- Updating all v2 message transport tests and examples
- Updating README and rustdoc documentation
- Adding no-hidden-spawn regression check
- Provider validation on RXE and SIW
- Creating PAW documentation artifact (Docs.md)

Out of Scope:
- V1 API changes of any kind
- Implementing a custom executor, reactor, or event loop
- Multi-connection driver sharing (each connection gets its own driver)
- Adding new transport features beyond explicit spawning
- Async runtime abstraction layer (Tokio remains the primary supported runtime)
- Changing the low-level `FdCqDriver`/`PollingCqDriver` public `(driver, handle)` API contract

## Dependencies

- Tokio async runtime (existing dependency)
- RDMA CM and verbs kernel interfaces via rdma-core (existing dependency)
- RXE or SIW soft-RDMA provider for testing (existing test infrastructure)
- Merged PR #37 (v2 message transport) at commit c530ae0

## Risks & Mitigations

- **Driver future composition complexity**: Composing CQ driving, recv pumping, HELLO, disconnect monitor, and reclamation into one state machine may be complex. Mitigation: Use `tokio::select!` or equivalent within the driver's poll implementation; accept boxed futures for internal composition
- **Self-referential lifetime issues**: Pinning and composing multiple futures with shared state can create borrow issues. Mitigation: Use Arc-based shared state between frontend and driver; avoid self-referential structs
- **HELLO deadlock risk**: If both peers wait for HELLO response before making progress, deadlock can occur. Mitigation: Pre-post receive buffers before returning the pair; driver sends HELLO and processes incoming HELLO concurrently
- **Provider-specific behavior differences**: RXE and SIW may handle edge cases differently. Mitigation: Test on both providers; fix provider-specific hangs with bounded timeouts
- **Breaking v2 API consumers**: Changing return types is a breaking change. Mitigation: This is explicitly an intentional v2 evolution; document the change clearly

## References

- Issue: none
- Research: .paw/work/v2-explicit-driver-spawning/SpecResearch.md (inline from initialization)
- Prior art: PR #37 (v2 message transport), PR #36 (v2 RDMA API)
