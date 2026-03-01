# Feature Specification: Async RDMA Integration

**Branch**: feature/async-rdma-integration  |  **Created**: 2026-03-01  |  **Status**: Draft
**Input Brief**: Add async/await support to rdma-io, enabling RDMA operations to participate in Rust async event loops without spin-polling.

## Overview

Today, `rdma-io` provides a synchronous API where completion queue (CQ) polling uses `thread::yield_now()` spin loops. This works but wastes CPU and cannot compose with async runtimes — a server handling RDMA alongside TCP, timers, and other I/O sources must dedicate threads to RDMA polling.

RDMA hardware provides a built-in notification mechanism: `ibv_comp_channel` exposes a file descriptor that becomes readable when a completion arrives. By registering this fd with an async runtime's reactor (epoll/kqueue), RDMA completions become regular awaitable events — zero CPU when idle, automatic wakeup on completion.

This feature adds async support as a layered, opt-in addition to the existing sync API. The core abstraction is `AsyncCq`, which wraps a completion channel fd with a runtime-agnostic notifier trait. On top of that, `AsyncRdmaStream` provides `futures::io::AsyncRead/AsyncWrite` for stream-oriented workloads, while `AsyncQp` exposes individual RDMA verbs (READ, WRITE, SEND, RECV, atomics) as async methods for direct hardware access.

The design is runtime-agnostic: a `CqNotifier` trait abstracts over tokio (`AsyncFd`) and smol (`async_io::Async`), with each runtime adapter behind a cargo feature flag. Users who don't need async pay zero cost — the sync API is unaffected.

## Objectives

- Enable RDMA I/O to participate in async event loops alongside other I/O sources without spin-polling
- Provide `futures::io::AsyncRead`/`AsyncWrite` on RDMA streams for composability with standard async ecosystem tools
- Expose every RDMA verb (READ, WRITE, SEND, RECV, atomics) as an individual async operation
- Support multiple async runtimes (tokio, smol) without code duplication via trait abstraction
- Keep async support opt-in behind feature flags — zero impact on sync users
- Maintain the library's layered architecture: users choose their abstraction level (stream vs. verb vs. raw CQ)

## User Scenarios & Testing

### User Story P1 – Async Stream I/O
Narrative: A developer building an async RPC server uses `AsyncRdmaStream` to handle RDMA connections within their tokio event loop, reading and writing data with `.await` just like TCP streams.

Independent Test: Connect two async RDMA endpoints, send a message, receive the echo — all within a tokio runtime.

Acceptance Scenarios:
1. Given a tokio runtime, When a client connects via `AsyncRdmaStream::connect()` and writes data, Then the server receives the data via `.read().await` without blocking any OS threads.
2. Given an established async stream, When the user wraps it with `tokio_util::compat::FuturesAsyncReadCompatExt`, Then `tokio::io::copy` works correctly for zero-copy forwarding.
3. Given an async listener, When multiple clients connect concurrently, Then each connection is handled as a separate async task without thread-per-connection overhead.

### User Story P2 – Async CQ Polling
Narrative: A low-level developer needs async completion notification without the stream abstraction. They create an `AsyncCq` and await completions after posting work requests manually.

Independent Test: Create an `AsyncCq` with a tokio notifier, post a SEND WR, and await the completion — the future resolves with the work completion entry.

Acceptance Scenarios:
1. Given an `AsyncCq` backed by a `TokioCqNotifier`, When a WR completes, Then `async_cq.poll().await` returns the completion without spin-polling.
2. Given an `AsyncCq` with no pending completions, When `poll().await` is called, Then the task yields to the runtime (returns `Pending`) and wakes when a completion arrives.
3. Given multiple completions arriving in a burst, When `poll().await` is called, Then all available completions are drained in one batch (drain-after-arm pattern).

### User Story P3 – Async RDMA Verbs
Narrative: A developer building a distributed key-value store uses `AsyncQp` to perform RDMA READ/WRITE operations directly to remote memory, with each operation as an awaitable future.

Independent Test: Perform an async RDMA WRITE to remote memory, then an async RDMA READ back, and verify the data matches.

Acceptance Scenarios:
1. Given an `AsyncQp` with a connected RC QP, When `write_remote().await` completes, Then the data is written to the remote memory region.
2. Given an `AsyncQp`, When `read_remote().await` completes, Then the local buffer contains the remote data.
3. Given an `AsyncQp`, When `compare_and_swap().await` completes, Then the return value is the original remote value.

### User Story P4 – Runtime Agnostic
Narrative: A library author uses `rdma-io` with the smol runtime instead of tokio. They expect the same async API to work with a different feature flag.

Independent Test: Run the async echo test using smol's executor instead of tokio — it passes without code changes to the RDMA layer.

Acceptance Scenarios:
1. Given `features = ["smol"]` in Cargo.toml, When `AsyncRdmaStream::connect_smol()` is called, Then the connection succeeds and data flows via smol's reactor.
2. Given the `CqNotifier` trait, When a user implements a custom notifier for a third-party runtime, Then `AsyncCq` works with that runtime.

### Edge Cases
- Spurious wakeup: fd reports readable but `ibv_get_cq_event` returns `EAGAIN` — must retry without error.
- Completion between arm and await: drain-after-arm pattern catches completions that arrive after `req_notify` but before `readable().await`.
- CQ event ack overflow: `ibv_ack_cq_events` must be called periodically (every ~16 events) to prevent internal counter overflow.
- Drop during pending operation: dropping an `AsyncRdmaStream` mid-read must not leak resources or panic — posted WRs are cleaned up by QP destruction.
- Non-blocking fd setup failure: if `fcntl(O_NONBLOCK)` fails, `CompletionChannel::new()` returns an error.

## Requirements

### Functional Requirements
- FR-001: `CompletionChannel` type wrapping `ibv_comp_channel` with non-blocking fd and `AsRawFd` (Stories: P2)
- FR-002: `CqNotifier` trait with `readable()` method returning a boxed future (Stories: P2, P4)
- FR-003: `TokioCqNotifier` implementing `CqNotifier` via `tokio::io::unix::AsyncFd` (Stories: P1, P2)
- FR-004: `SmolCqNotifier` implementing `CqNotifier` via `async_io::Async` (Stories: P4)
- FR-005: `AsyncCq` with `poll()` and `poll_wr_id()` async methods using drain-after-arm pattern (Stories: P1, P2, P3)
- FR-006: `AsyncRdmaStream` with async `read()`/`write()` methods (Stories: P1)
- FR-007: `AsyncRdmaListener` with async `bind()`/`accept()` methods (Stories: P1)
- FR-008: `futures::io::AsyncRead` + `AsyncWrite` trait implementations on `AsyncRdmaStream` (Stories: P1)
- FR-009: `AsyncQp` with async methods for SEND, RECV, RDMA READ, RDMA WRITE, WRITE_WITH_IMM (Stories: P3)
- FR-010: `AsyncQp` with async atomic verbs: `compare_and_swap()`, `fetch_and_add()` (Stories: P3)
- FR-011: Batch operation support via `post_send_batch()` (Stories: P3)
- FR-012: Feature-gated dependencies: `tokio` feature enables TokioCqNotifier, `smol` feature enables SmolCqNotifier (Stories: P4)
- FR-013: CQ event ack batching (every ~16 events) with ack-all-remaining in Drop (Stories: P2)
- FR-014: `CompletionQueue::with_comp_channel()` constructor associating a CQ with a completion channel (Stories: P2)

### Key Entities
- `CompletionChannel`: Safe wrapper around `ibv_comp_channel`, owns the fd, Drop destroys it
- `CqNotifier`: Trait abstracting async fd readiness across runtimes
- `AsyncCq`: Core async CQ poller combining CQ + channel + notifier
- `AsyncRdmaStream`: High-level async stream (SEND/RECV with buffering)
- `AsyncRdmaListener`: Async connection acceptor
- `AsyncQp`: Mid-level async wrapper exposing all RDMA verbs
- `RemoteMr`: Token (addr, len, rkey) for one-sided RDMA operations

### Cross-Cutting / Non-Functional
- Existing sync API must remain completely unaffected when async features are not enabled
- All async types must be `Send` (required for `tokio::spawn`)
- No background threads — all async operations use fd-based reactor notification
- CQ event ack batching to amortize mutex cost in `ibv_ack_cq_events`

## Success Criteria
- SC-001: Async echo test passes — client sends data, server echoes it back, all via `.await` on a tokio runtime (FR-005, FR-006, FR-007)
- SC-002: `AsyncCq::poll().await` returns completions without spin-polling — verified by CPU usage or by confirming the task yields to the runtime (FR-005)
- SC-003: `futures::io::AsyncRead`/`AsyncWrite` trait impls work with `tokio::io::copy` via compat layer (FR-008)
- SC-004: Async RDMA READ/WRITE roundtrip succeeds — write remote then read back, data matches (FR-009)
- SC-005: Building with `default-features = false` (no async features) compiles without async dependencies (FR-012)
- SC-006: SmolCqNotifier passes the same echo test on the smol runtime (FR-004)
- SC-007: Dropping an `AsyncCq` correctly acks all outstanding CQ events (FR-013)
- SC-008: All async types compile as `Send` — verified via compile-time assertion (Cross-cutting)

## Assumptions
- siw (SoftIWarp) supports `ibv_comp_channel` and non-blocking fd operations — this will be verified in Phase A
- `ibv_get_cq_event` returns `EAGAIN` (not blocks) when the fd is non-blocking and no event is pending
- The `rdma_event_channel.fd` can be set non-blocking and polled with epoll (for async CM in Phase E)
- tokio 1.x and async-io 2.x are acceptable minimum versions for runtime adapters
- Atomic verbs (CAS, FAA) may not work on siw — tests will be skipped if unsupported

## Scope

In Scope:
- `CompletionChannel`, `CqNotifier` trait, `TokioCqNotifier`, `SmolCqNotifier`
- `AsyncCq` with drain-after-arm pattern
- `AsyncRdmaStream` and `AsyncRdmaListener` with async methods
- `futures::io::AsyncRead`/`AsyncWrite` trait implementations
- `AsyncQp` with all RDMA verbs (SEND, RECV, READ, WRITE, atomics)
- Feature flags: `tokio`, `smol`
- Async CM event channel for fully non-blocking connection setup

Out of Scope:
- `tokio::io::AsyncRead` direct implementation (use compat layer instead)
- io_uring integration
- Async device events (`ibv_get_async_event`)
- Custom event loop / reactor
- Shared Receive Queue (SRQ) async support
- Memory Window (MW) support
- Timeouts, graceful shutdown, stream split (future work)

## Dependencies
- `futures-io` 0.3 — `AsyncRead`/`AsyncWrite` traits (always, when async enabled)
- `tokio` 1.x — `AsyncFd`, `spawn_blocking` (behind `tokio` feature)
- `tokio-util` 0.7 — `FuturesAsyncReadCompatExt` (behind `tokio` feature)
- `async-io` 2.x — `Async<T>` fd wrapper (behind `smol` feature)
- Existing: `libibverbs-dev`, `librdmacm-dev` system libraries
- Existing: siw kernel module for testing

## Risks & Mitigations
- siw may not support `ibv_comp_channel` properly: Mitigation — verify in Phase A; if broken, test on rxe or real hardware.
- `ibv_get_cq_event` may block even with non-blocking fd: Mitigation — only call after `epoll` confirms readability; add integration test to verify.
- Cancellation safety of RDMA operations: Mitigation — document which ops are safe to cancel; posted WRs are cleaned up by QP destroy.
- `futures::io` compat layer overhead: Mitigation — verify via benchmarks that `.compat()` is zero-cost.

## References
- Design: docs/design/AsyncIntegration.md (comprehensive async integration design)
- Design: docs/design/SafeApi.md (master safe API design)
- Prior art: async-rdma crate (DatenLord) — different approach (tokio-only, agent-based)
