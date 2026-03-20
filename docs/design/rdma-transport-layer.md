# RDMA Transport Layer — Architecture

**Status:** Implemented  
**Date:** 2026-03-14  
**Source code:** `rdma-io/src/transport.rs`, `rdma-io/src/send_recv_transport.rs`, `rdma-io/src/async_stream.rs`

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
          ┌────────────┼──────────────────────┐
          │            │                      │
  SendRecvTransport  CreditRingTransport    ReadRingTransport
  (Send/Recv)        (Write+Ring+Credits)   (Write+Ring+Read)
          │            │                      │
          └────────────┴──────────────────────┘
                       │
              ring_common (shared ring infrastructure)
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

## SendRecvTransport

Concrete `impl Transport` using RDMA Send/Recv. Owns all resources:

| Resource | Details |
|----------|---------|
| QP | RC, dual CQ (send + recv separate) |
| Send buffers | `Vec<OwnedMemoryRegion>`, round-robin with `send_in_flight: Vec<bool>` |
| Recv buffers | `Vec<OwnedMemoryRegion>`, pre-posted on creation |
| CM | `CmId` + `EventChannel` + `AsyncFd<RawFd>` for waker registration |
| Config | `SendRecvConfig` with `stream()` and `datagram()` presets |

**Drop order is critical** (Rust drops fields in declaration order):
state → MRs → PD → QP → cm_async_fd → cm_id → event_channel

Construction: `SendRecvTransport::connect(addr, config)` / `SendRecvTransport::accept(listener, config)`

## AsyncRdmaStream\<T: Transport\>

Generic byte stream consuming the trait.

| Field | Purpose |
|-------|---------|
| `transport: T` | The underlying transport |
| `recv_pending: Option<(buf_idx, offset, total_len)>` | Partial read state |
| `write_pending: Option<usize>` | In-flight send length |
| `eof: bool` | Transport errored → stop polling |

Implements: `AsyncRead`, `AsyncWrite`, `Unpin`, `Send`, `Sync`, `Debug`, `Drop`

Construction: `AsyncRdmaStream::new(transport)` — always takes an explicit transport instance.

## TransportBuilder Trait

`TransportBuilder: Clone + Send + Sync + Unpin + 'static` — config-as-builder for connection
establishment. Associated type `Transport: Transport + 'static`.

| Method | Purpose |
|--------|---------|
| `connect(&self, addr) → impl Future<Output = Result<Self::Transport>> + Send` | Client-side connect |
| `accept(&self, listener) → impl Future<Output = Result<Self::Transport>> + Send` | Server-side accept |

Implementations:
- `SendRecvConfig` → `SendRecvTransport` (Send/Recv, presets: `stream()`, `datagram()`)
- `CreditRingConfig` → `CreditRingTransport` (Write+Imm ring buffers, presets: `datagram()`, `default()`)

Generic consumers (e.g., `RdmaIncoming<B>`, `RdmaConnector<B>`, `RdmaUdpSocket<B>`) accept
any builder — users choose the transport at construction time.

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

- ✅ **Phase 1: Transport Trait + SendRecvTransport** — `transport.rs`, `send_recv_transport.rs`, refactored `async_stream.rs`.
- ✅ **Phase 1.5: poll_get_request** — Added to `AsyncCmListener` for non-blocking accept in `poll_recv`.
- ✅ **Phase 2: RdmaUdpSocket** — `rdma-io-quinn` crate. Generic over `TransportBuilder`. Quinn echo + multi-peer tests pass.
- ✅ **Phase 3: CreditRingTransport** — Ring buffer transport via RDMA Write + Immediate Data with credit-based flow control.
- ✅ **Phase 4: TransportBuilder** — Generic builder trait. All consumers (`RdmaIncoming`, `RdmaConnector`, `RdmaUdpSocket`, `TokioRdmaStream`) are generic over the builder.
- ✅ **Phase 5: Generic tests** — All stream, tonic, quinn, and h3 tests run on both Send/Recv and Ring transports.

## Ring Buffer Transport

`CreditRingTransport` implements `Transport` using RDMA Write + Immediate Data.
Drop-in replacement via generics — consumer code unchanged.
See [rdma-credit-ring-transport.md](rdma-credit-ring-transport.md) for the full design.

| Aspect | SendRecvTransport | CreditRingTransport | ReadRingTransport |
|--------|--------------|-------------------|------------------|
| Verb | `ibv_post_send(SEND)` | `ibv_post_send(WRITE_WITH_IMM)` | `ibv_post_send(WRITE_WITH_IMM)` |
| Recv mechanism | Pre-posted recv buffers | Doorbell + ring landing zone | Doorbell + ring landing zone |
| Flow control | Hardware RNR (always accepts) | Credit-based (Send+Imm) | RDMA Read (offset buffer) |
| `send_copy` backpressure | `Ok(n)` always | `Ok(0)` when credits exhausted | `Ok(0)` when cached head full |
| Receiver WRs/msg | 1 post_recv | 1 Send+Imm + 1 post_recv | 0 Send + 1 post_recv |
| Stream HOL | None (independent buffers) | Yes (ring Head blocks) | Yes (ring Head blocks) |
| Memory Windows | None (two-sided) | 1 MW (recv ring) | 2 MWs (recv ring + offset buf) |

## Transport Naming Convention

The `Rdma` prefix is dropped since everything is in the `rdma_io` crate namespace.
Names describe the transport mechanism:

| Name | Mechanism |
|------|-----------|
| `SendRecvTransport` | RDMA Send/Recv, discrete buffers |
| `CreditRingTransport` | Write+Imm ring, credit-based flow control |
| `ReadRingTransport` | Write+Imm ring, RDMA Read flow control |
| `SendRecvConfig` | Builder for SendRecvTransport |
| `CreditRingConfig` | Builder for CreditRingTransport |
| `ReadRingConfig` | Builder for ReadRingTransport |

The naming pattern is **mechanism** + `Transport` / `Config`:
- `SendRecv` — the RDMA verbs used for data transfer
- `CreditRing` — ring buffer with credit-based flow control
- `ReadRing` — ring buffer with RDMA Read flow control

See [RingPerformance.md](../future/RingPerformance.md) for the transport family
design and trade-off analysis.

## References

- [quinn-rdma.md](quinn-rdma.md) — Quinn RDMA integration (implemented)
- [rdma-credit-ring-transport.md](rdma-credit-ring-transport.md) — CreditRingTransport design (implemented)
- [rdma-read-ring-transport.md](rdma-read-ring-transport.md) — ReadRingTransport design (implemented)
- [rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way comparison
- [rdma-io/src/transport.rs](../../rdma-io/src/transport.rs) — Transport trait
- [rdma-io/src/send_recv_transport.rs](../../rdma-io/src/send_recv_transport.rs) — SendRecvTransport
- [rdma-io/src/credit_ring_transport.rs](../../rdma-io/src/credit_ring_transport.rs) — CreditRingTransport
- [rdma-io/src/read_ring_transport.rs](../../rdma-io/src/read_ring_transport.rs) — ReadRingTransport
- [rdma-io/src/ring_common.rs](../../rdma-io/src/ring_common.rs) — Shared ring infrastructure
- [rdma-io/src/async_stream.rs](../../rdma-io/src/async_stream.rs) — AsyncRdmaStream\<T\>
- [rdma-io-quinn/src/lib.rs](../../rdma-io-quinn/src/lib.rs) — RdmaUdpSocket (Quinn integration)
