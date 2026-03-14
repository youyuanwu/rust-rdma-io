# RDMA Transport Layer — Architecture

**Status:** Implemented  
**Date:** 2026-03-14  
**Source code:** `rdma-io/src/transport.rs`, `rdma-io/src/rdma_transport.rs`, `rdma-io/src/async_stream.rs`

## Overview

The Transport trait abstracts RDMA data-path mechanics (QP, CQ, buffers, CM, disconnect)
behind a consumer-facing interface. Both byte-stream (gRPC/tonic) and datagram (Quinn/QUIC)
consumers build on it via generics — zero vtable overhead, mock-testable.

```
AsyncRdmaStream<T> (byte stream)      RdmaUdpSocket<T> (datagram, future)
  ├── Partial read buffering             ├── Multi-peer connection map
  ├── AsyncRead / AsyncWrite             ├── AsyncUdpSocket / UdpSender
  └── Graceful close                     └── Batch recv
        │                                      │
        └──────────────┬───────────────────────┘
                       │
              Transport trait (10 methods)
                       │
              RdmaTransport (Send/Recv)
                ├── QP + dual CQ
                ├── Configurable buffer pool
                ├── In-flight send tracking
                ├── CM disconnect detection + QP state fallback
                └── Connection lifecycle (connect / accept)
                       │
              AsyncQp / AsyncCq / AsyncCmId (primitives)
```

## Transport Trait

10 methods, 1 associated type (`RecvCompletion`). No RDMA-specific types in signatures.

| Method | Purpose |
|--------|---------|
| `send_copy(&mut self, data) → Result<usize>` | Copy + post send. Returns `Ok(0)` if all buffers occupied. |
| `poll_send_completion(cx) → Poll<Result<()>>` | Wait for ≥1 send to complete. Err on CQ failure. |
| `poll_recv(cx, out) → Poll<Result<usize>>` | Batched recv completions → `RecvCompletion { buf_idx, byte_len }` |
| `recv_buf(buf_idx) → &[u8]` | Access received data (valid until repost) |
| `repost_recv(buf_idx) → Result<()>` | Release recv buffer for reuse |
| `poll_disconnect(cx) → bool` | Register CM waker + check state. True = dead. |
| `is_qp_dead() → bool` | Non-blocking dead check |
| `disconnect() → Result<()>` | Initiate graceful disconnect (idempotent) |
| `local_addr() / peer_addr()` | Connection metadata |

**Key design decisions:**
- `send_copy` manages buffers internally (round-robin with in-flight tracking)
- `poll_recv`/`poll_send_completion` convert WC errors to `Err()` — consumers never see `WorkCompletion`
- `poll_disconnect` includes `is_qp_dead()` fallback for rxe (QP transitions without CM event)
- `repost_recv(&mut self)` — compatible with future Ring transport that mutates internal state

## RdmaTransport

Concrete `impl Transport` using RDMA Send/Recv. Owns all resources:

| Resource | Details |
|----------|---------|
| QP | RC, dual CQ (send + recv separate) |
| Send buffers | `Vec<OwnedMemoryRegion>`, round-robin with `send_in_flight: Vec<bool>` |
| Recv buffers | `Vec<OwnedMemoryRegion>`, pre-posted on creation |
| CM | `CmId` + `EventChannel` + `AsyncFd<RawFd>` for waker registration |
| Config | `TransportConfig` with `stream()` and `datagram()` presets |

**Drop order is critical** (Rust drops fields in declaration order):
state → MRs → PD → QP → cm_async_fd → cm_id → event_channel

Construction: `RdmaTransport::connect(addr, config)` / `RdmaTransport::accept(listener, config)`

## AsyncRdmaStream\<T: Transport\>

Generic byte stream consuming the trait. Default type parameter `= RdmaTransport`.

| Field | Purpose |
|-------|---------|
| `transport: T` | The underlying transport |
| `recv_pending: Option<(buf_idx, offset, total_len)>` | Partial read state |
| `write_pending: Option<usize>` | In-flight send length |
| `eof: bool` | Transport errored → stop polling |

Implements: `AsyncRead`, `AsyncWrite`, `Unpin`, `Send`, `Sync`, `Debug`, `Drop`

Convenience: `AsyncRdmaStream::connect(addr)` hides the generic parameter.

## Responsibility Split

| Concern | Transport | Stream | UdpSocket (future) |
|---------|-----------|--------|-------------------|
| QP/CQ/MR lifecycle | ✓ | — | — |
| WC → completion conversion | ✓ | — | — |
| Send buffer management | ✓ | — | — |
| Disconnect detection | ✓ | calls | calls |
| Partial read buffering | — | ✓ | — |
| Graceful close | — | ✓ | — |
| Multi-peer map | — | — | ✓ |
| Dead connection cleanup | — | — | ✓ |

## Migration Status

- ✅ **Phase 1: Transport Trait + RdmaTransport** — `transport.rs` (114 lines), `rdma_transport.rs` (448 lines), refactored `async_stream.rs` (404 lines). 44 tests pass, zero warnings.
- ✅ **Phase 1.5: poll_get_request** — Added to `AsyncCmListener` for non-blocking accept in `poll_recv`.
- ✅ **Phase 2: RdmaUdpSocket** — `rdma-io-quinn` crate (~240 lines). Quinn echo test passes. 45 total tests.
- ⬜ **Phase 3: Optimize** — SRQ, inline data, UD QP, Ring transport

## Future: Ring Buffer Transport

`RdmaRingTransport` would implement `Transport` using RDMA Write + Immediate Data.
Drop-in replacement via generics — consumer code unchanged.
See [rdma-ring-transport.md](rdma-ring-transport.md) for the full design.

| Aspect | RdmaTransport | RdmaRingTransport |
|--------|--------------|-------------------|
| Verb | `ibv_post_send(SEND)` | `ibv_post_send(WRITE_WITH_IMM)` |
| Recv mechanism | Pre-posted recv buffers | Doorbell + ring landing zone |
| `send_copy` backpressure | Hardware RNR (always accepts) | Credit-based (`Ok(0)` when full) |
| Stream HOL | None (independent buffers) | Yes (ring Head blocks) |

## References

- [quinn-rdma.md](quinn-rdma.md) — Quinn RDMA integration (implemented)
- [rdma-ring-transport.md](rdma-ring-transport.md) — Ring buffer transport design (future)
- [rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way comparison
- [rdma-io/src/transport.rs](../../rdma-io/src/transport.rs) — Transport trait
- [rdma-io/src/rdma_transport.rs](../../rdma-io/src/rdma_transport.rs) — RdmaTransport
- [rdma-io/src/async_stream.rs](../../rdma-io/src/async_stream.rs) — AsyncRdmaStream\<T\>
- [rdma-io-quinn/src/lib.rs](../../rdma-io-quinn/src/lib.rs) — RdmaUdpSocket (Quinn integration)
