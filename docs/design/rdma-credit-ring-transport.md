# RDMA Ring Buffer Transport

**Status:** Implemented  
**Updated:** 2026-03-15  
**Implementation:** `rdma-io/src/credit_ring_transport.rs`  
**Reference:** [rdma-transport-layer.md](rdma-transport-layer.md) — Transport architecture  
**Reference:** [rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way comparison  
**Reference:** [rdma-read-ring-transport.md](rdma-read-ring-transport.md) — ReadRingTransport (sibling)  
**Reference:** [msquic-rdma.md](../background/msquic-rdma.md) — msquic Write+Ring analysis  
**Reference:** [Rsocket.md](../background/Rsocket.md) — rsocket Write+Ring analysis

## Overview

`CreditRingTransport` implements the `Transport` trait using RDMA Write + Immediate Data
instead of Send/Recv — the same approach as msquic and rsocket. Since `AsyncRdmaStream<T>` is
generic over `T: Transport`, it is a **drop-in replacement**:

```rust
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::transport::TransportBuilder;

// Via TransportBuilder (recommended)
let transport = CreditRingConfig::default().connect(&addr).await?;
let stream = AsyncRdmaStream::new(transport);  // same interface, different engine

// Or directly
let transport = CreditRingTransport::connect(&addr, CreditRingConfig::default()).await?;
let stream = AsyncRdmaStream::new(transport);
```

**Best for:** QUIC datagrams, bulk transfer on InfiniBand/RoCE.  
**Not for:** iWARP/siw (no Write+Imm support), byte streams (ring HOL blocking).

## Architecture

| Operation | `SendRecvTransport` (Send/Recv) | `CreditRingTransport` (Write+Ring) |
|-----------|---------------------------|----------------------------------|
| `send_copy()` | Copy → send buf, `post_send(SEND)` | Check credit → copy → send_ring, `post_send(WRITE_WITH_IMM)` |
| `poll_recv()` | Poll RecvCQ → `RecvCompletion` | Poll RecvCQ for doorbells, decode imm → virtual `RecvCompletion` |
| `recv_buf(idx)` | `recv_bufs[idx].as_slice()` | `recv_ring[offset..offset+len]` via `virtual_idx_map` |
| `repost_recv(idx)` | `ibv_post_recv` on buffer idx | Advance ring head, repost doorbell, send credit update |
| Backpressure | `Ok(0)` when all send bufs in-flight | `Ok(0)` when remote credits exhausted |
| Setup | Create QP, alloc N×MR per direction | Create QP, alloc rings, exchange tokens (VA + rkey + capacity) |

The **virtual buffer index** mapping is the key abstraction: `poll_recv` decodes each
doorbell's immediate data → `(offset, length)`, assigns a `virt_idx` from a bounded slab
(`Vec<Option<(usize, usize)>>`), and returns `RecvCompletion { buf_idx: virt_idx }`.
Consumers use `virt_idx` identically to discrete buffer indices.

## Client-Server Data Flow

### Write Path (Client → Server)

```
  Client                                     Server
    │                                          │
    │  send_copy("hello")                      │
    │    1. Check remote_credits > 0           │
    │    2. Copy data → send_ring[tail]        │
    │    3. Post RDMA Write+Imm               │
    │       imm = (remote_offset<<16)|len      │
    │       remote_addr = server recv_ring     │
    │                                          │
    │  ═══ RDMA Write (NIC DMA) ══════════════►│ Data lands directly in recv_ring
    │  ═══ Immediate Data (doorbell) ═════════►│ Recv CQ completion (RecvRdmaWithImm)
    │                                          │
    │  poll_send_completion()                   │  poll_recv()
    │    4. Send CQ → completion               │    5. Decode imm → (offset, len)
    │    5. Release send_ring space             │    6. Assign virtual buf_idx
    │       send_ring.release(len)             │    7. Return RecvCompletion
    │                                          │
    │                                          │  recv_buf(buf_idx)
    │                                          │    8. Read recv_ring[offset..offset+len]
    │                                          │
    │                                          │  repost_recv(buf_idx)
    │                                          │    9. Release recv_ring space
    │                                          │   10. Repost doorbell recv WR
    │                                          │   11. Send credit update (SendWithImm)
    │                                          │
    │  ◄══ Credit Update (Send+Imm) ══════════│
    │  poll_recv() or drain_recv_credits()     │
    │   12. Decode credit → remote_credits++   │
    │                                          │
```

### Full Echo Round-Trip

```
  Client                          Server
    │                               │
    │ ── Write+Imm("request") ────►│  1. Client sends data
    │                               │  2. Server poll_recv → data
    │                               │  3. Server recv_buf → read
    │                               │  4. Server repost_recv → credit
    │ ◄── SendWithImm(credit) ─────│
    │                               │
    │                               │  5. Server send_copy("response")
    │ ◄── Write+Imm("response") ───│  6. Server writes to client ring
    │                               │
    │  7. Client poll_recv → data   │
    │  8. Client recv_buf → read    │
    │  9. Client repost_recv        │
    │ ── SendWithImm(credit) ─────►│ 10. Credit returned to server
    │                               │
```

### Credit Exhaustion Recovery

```
  Client (sender)                  Server (receiver, slow consumer)
    │                               │
    │ ── Write+Imm (msg 1) ───────►│  remote_credits: 43→42
    │ ── Write+Imm (msg 2) ───────►│  remote_credits: 42→41
    │    ... (41 more sends) ...    │
    │ ── Write+Imm (msg 43) ──────►│  remote_credits: 1→0
    │                               │
    │  send_copy(msg 44)            │  (server hasn't called repost_recv yet)
    │    remote_credits == 0!       │
    │    drain_recv_credits()       │
    │    → poll recv CQ (no-op)     │
    │    → still 0 credits          │
    │    → return Ok(0)             │
    │                               │
    │  (AsyncRdmaStream retries     │  Server finally reads and reposts:
    │   via poll_send_completion)   │    repost_recv() × 5
    │                               │    → 5 credit updates sent
    │                               │
    │ ◄── SendWithImm(credits=5) ──│
    │  drain_recv_credits()         │
    │    remote_credits: 0→5        │
    │  send_copy(msg 44) succeeds!  │
    │ ── Write+Imm (msg 44) ──────►│
    │                               │
```

## Credit Protocol

Credit-based flow control prevents the sender from overwriting unread data in the
receiver's ring buffer.

**Absolute encoding.** Each `repost_recv()` sends a `SendWithImm` carrying the **total
freed credits** as a `u32` immediate value. The sender computes `delta =
freed_count.wrapping_sub(last_received)` to update `remote_credits`. If a credit WR
is lost (QP error), the next successful one recovers all lost credits — no permanent stall.

**Send+Imm (not Write+Imm).** Credits use `WrOpcode::SendWithImm`, not RDMA Write. The
receiver distinguishes the two by completion opcode:
- `WcOpcode::RecvRdmaWithImm` → data or padding (RDMA Write landed)
- `WcOpcode::Recv` with `IBV_WC_WITH_IMM` flag → credit update (Send landed)

**Inline credit drain in `send_copy`.** When `remote_credits == 0`, `send_copy` calls
`drain_recv_credits()` — a non-blocking poll of the recv CQ that processes any pending
credit completions (and stashes data completions for the next `poll_recv`). This allows
the transport to recover credits without requiring the caller to explicitly poll the recv
side, making it compatible with `AsyncRdmaStream` without stream-layer changes.

**Credit exhaustion sequence:**
1. `send_copy(data)` checks `remote_credits == 0`
2. Calls `drain_recv_credits()` — arms recv CQ, polls once, processes any completions
3. If credits recovered → proceeds with the send normally
4. If still 0 → returns `Ok(0)` (caller should `poll_send_completion` and retry)
5. `AsyncRdmaStream::poll_write` handles `Ok(0)` by waiting on `poll_send_completion`,
   then retrying `send_copy` — which calls `drain_recv_credits` again
6. This retry loop converges once the peer's `repost_recv` credit updates arrive

**WR identification.** WR IDs use bit 63 as a flag: `0` = data/padding WR, `1` = credit
WR. The padding sentinel is `u64::MAX - 20`. Data WR IDs encode the total bytes to release
(padding + data length) in the lower bits for direct `send_ring.release()` on completion.

## Immediate Data Encoding

Small-ring mode (≤64 KB): `imm = (offset << 16) | length` — 16 bits each.

- **Data:** `offset` is the byte position in the remote ring; `length` is the payload size.
- **Padding:** `length = 0` signals a wrap-around padding marker. The receiver advances
  `recv_ring.head` by `capacity - offset` to skip the gap.

Max message length is 65535 bytes (16-bit). `send_copy` clamps via `.min(0xFFFF)`.

## Connection Setup

Token exchange uses Send/Recv to share ring metadata. The 20-byte `RingToken`
(`version`, `ring_va`, `mw_rkey`, `capacity`) is sent inline.

**WR ordering is critical:** the token Recv WR is posted BEFORE `connect()`/`accept()`
(before doorbell WRs) so the 20-byte token Send from the peer is consumed by the token
Recv, not by a 4-byte doorbell buffer. Doorbell Recv WRs are posted AFTER token exchange.

**Async wait:** `complete_token_exchange` uses `AsyncCq::poll().await` on the recv CQ
for the token arrival, with a configurable timeout (default 5s via `CreditRingConfig::token_timeout`).

**Multi-connection support:** When multiple clients connect to the same listener
concurrently, `ConnectRequest` events can interleave with `Established` events on the
listener's shared event channel. `AsyncCmListener::complete_accept` stashes interleaved
`ConnectRequest` events in `pending_requests`, and `get_request`/`poll_get_request`
drain the stash before polling the event channel.

```
  Client                              Server
    │ Resolve addr                       │ get_request()
    │ Alloc rings, register MRs          │ Alloc rings, register MRs
    │ Post token Recv WR                 │ Post token Recv WR
    │ rdma_connect()                     │ complete_accept()
    │ ── QP reaches RTS ──               │ ── QP reaches RTS ──
    │ Alloc MW Type 2                    │ Alloc MW Type 2
    │ Post IBV_WR_BIND_MW               │ Post IBV_WR_BIND_MW
    │  (MW → recv_ring MR region)        │  (MW → recv_ring MR region)
    │                                    │
    │ Send(RingToken{mw_rkey}) ────────►│ Parse → store remote info
    │◄──────────────────────────────── │ Send(RingToken{mw_rkey})
    │ Parse → store remote info          │
    │                                    │
    │ Post doorbell Recv WRs             │ Post doorbell Recv WRs
    │ Ready ◄────────────────────────────► Ready
```

## TransportBuilder Integration

`CreditRingConfig` implements `TransportBuilder`, enabling generic usage across the stack:

```rust
use rdma_io::credit_ring_transport::CreditRingConfig;

// tonic gRPC server + client
let incoming = RdmaIncoming::bind(&addr, CreditRingConfig::default())?;
let connector = RdmaConnector::new(CreditRingConfig::default());

// Quinn QUIC socket
let socket = RdmaUdpSocket::bind(&addr, CreditRingConfig::datagram())?;

// Generic test helper
async fn echo_test<B: TransportBuilder>(builder: B) { ... }
echo_test(CreditRingConfig::default()).await;
```

## Drop Safety

Resource cleanup relies on Rust's field declaration order. The `Drop` impl drains both
CQs before fields are dropped, ensuring all in-flight WRs release MR references:

1. **`Drop` impl** — disconnect (if not already), drain send + recv CQs to completion
2. `_recv_mw` (MW) — deallocated before QP; kernel implicitly invalidates on dealloc
3. `qp` (QP + CQs) — kernel flushes outstanding WRs
4. `send_ring`, `recv_ring` (MRs) — safe, all WR references cleared
5. `_pd`, `cm_async_fd`, `cm_id`, `event_channel` — CM resources drop last

## Design Decisions

### Adopted

| Feature | Source | Implementation |
|---------|--------|----------------|
| **Dual CQ** | msquic | Reused `AsyncQp` with separate send/recv CQs |
| **Small-ring immediate encoding** | msquic | `(offset<<16)\|length`, 64KB ring, no RDMA Read fallback |
| **Token exchange** | msquic | 20-byte `RingToken` via inline Send/Recv after connect |
| **Absolute credit encoding** | rsocket (adapted) | Total freed count in `SendWithImm` imm value — self-healing |
| **Memory Window Type 2** | msquic (adapted) | `IBV_WR_BIND_MW` scopes recv ring per-connection; panics if unsupported |
| **iWARP detection + reject** | rsocket | `any_device_is_iwarp()` check in `connect()`/`accept()` |
| **ConnectRequest stashing** | (original) | `pending_requests` in `AsyncCmListener` for multi-connection accept |

### Rejected

| Feature | Source | Reason |
|---------|--------|--------|
| **Single shared CQ** | rsocket | Mixed send/recv confusion; dual CQ validated by msquic |
| **13-state connection machine** | msquic | 3 implicit states suffice: setup → token exchange → ready |
| **IOCP/callback model** | msquic | Tokio `AsyncFd` + `poll_*` is fundamentally different |
| **Write+Imm for credits** | (original plan) | `SendWithImm` allows opcode-based disambiguation on recv CQ |
| **MR-only protection** | rsocket | MW Type 2 now implemented for per-connection scoping |

## Trade-offs

```
                     SendRecvTransport              CreditRingTransport
                     (Send/Recv)                (Write+Ring)
                ┌────────────────────┐    ┌────────────────────────┐
  Simplicity    │ ████████████████   │    │ ███                    │
  Portability   │ ████████████████   │    │ ████████               │
  Recv copies   │ ████ (2 copies)    │    │ ████████████████ (1)   │
  Throughput    │ ████████████       │    │ ████████████████       │
  Latency       │ ██████████████     │    │ ████████████████       │
  Code size     │ ████████████████   │    │ ██████                 │
  Safety        │ ████████████████   │    │ ████████████████ (MW) │
  Stream HOL    │ ████████████████   │    │ ████ (ring Head blocks)│
  iWARP         │ ████████████████   │    │ ██ (see below)         │
                └────────────────────┘    └────────────────────────┘

  Use SendRecvTransport:     byte streams (gRPC), iWARP/siw, many peers
  Use CreditRingTransport: QUIC datagrams, bulk transfer, InfiniBand/RoCE, few peers
```

## Known Limitations

- **MW Type 2 required.** `connect()`/`accept()` panic if `supports_mw_type2(&pd)` is
  false (device doesn't report `IBV_DEVICE_MEM_WINDOW_TYPE_2A/2B`). Some older rxe kernels
  report the flag but fail on `ibv_alloc_mw` — in that case, connect will error with a
  clear `ibv_alloc_mw` failure message.

- **Stream HOL blocking.** `AsyncRdmaStream` partial reads pin the ring head, blocking
  all subsequent messages. Prefer `SendRecvTransport` (Send/Recv) for byte streams.

- **No credit keepalive.** If the receiver stops consuming, the sender stalls at
  `remote_credits == 0` indefinitely. Relies on application-level timeouts (e.g. Quinn's
  idle timeout).

- **No credit batching.** Each `repost_recv` sends a credit update WR (~100% WR overhead
  for small messages). Batching (send every N reposts) is future optimization.

- **Max message 65535 bytes.** 16-bit length field in immediate data. Stream config's
  64 KiB (65536) overflows by 1 byte. Large-ring mode is deferred.

- **No iWARP support.** `connect()`/`accept()` return `Err` on iWARP devices. Callers
  should fall back to `SendRecvTransport`. Tests use `require_no_iwarp!()` macro.

- **`repost_recv` ordering.** Out-of-order repost delays ring head advancement (unlike
  `SendRecvTransport` where buffers are independent). Consumers holding recv buffers longer
  have different costs per transport.

## Implementation Notes

- **Memory Window bind ordering.** MW Type 2 bind via `IBV_WR_BIND_MW` send WR requires
  the QP to be in RTS state. The bind is posted AFTER `connect()`/`accept()` completes but
  BEFORE token exchange, so the token carries the MW rkey (not the MR rkey). The recv MR
  is registered with `MW_BIND` access flag. `supports_mw_type2(&pd)` checks device cap
  flags on the connection's actual device (routed via `rdma_cm`, not `open_first_device`).

- **`send_tracker` unused.** RC QPs guarantee in-order completions. `poll_send_completion`
  uses direct head advance: the data WR's `wr_id` encodes total bytes to release, and
  `send_ring.release(data_len)` advances the head. `CompletionTracker` is kept (with
  `#[allow(dead_code)]`) for potential future use with UD QPs.

- **`unsafe impl Send for SendWr`** (in `wr.rs`). `SendWr` contains `*mut ibv_mw` and
  `*mut ibv_mr` pointers (for MW bind fields). These are kernel-managed handles that
  don't alias mutable state, making `Send` safe.

- **Fail-fast for protocol violations.** Any invalid immediate data (offset/length out of
  bounds) sets `qp_dead = true` immediately. RC guarantees reliable delivery, so corruption
  means a bug, not a transient issue.

- **`remote_freed_received` wrapping.** Credit deltas use `wrapping_sub` on `u32` to
  handle counter overflow gracefully. Deltas > `max_outstanding` are ignored as stale.

- **Credit-only CQ re-poll in Quinn.** When `poll_recv` returns `Ok(0)` (credit-only batch),
  `poll_completions` returned `Ready` without registering a tokio waker on the CQ fd. Quinn's
  `RdmaUdpSocket::poll_recv` re-polls on `Ok(0)` to ensure the CQ waker is registered,
  preventing missed notifications under edge-triggered epoll.

- **`poll_read` loops on `Ok(0)`.** `AsyncRdmaStream::poll_read` re-polls the transport
  when `poll_recv` returns `Ok(0)` (credit-only), ensuring the CQ waker is properly
  registered before returning `Pending`.

## Files

| File | Description |
|------|-------------|
| `rdma-io/src/credit_ring_transport.rs` | Full implementation (~1300 lines) |
| `rdma-io/src/transport.rs` | `Transport` + `TransportBuilder` traits |
| `rdma-io/src/mw.rs` | `MemoryWindow` RAII wrapper |
| `rdma-io/src/wr.rs` | `SendWr`, `RecvWr`, `WrOpcode` (includes `SendWithImm`, `BindMw`) |
| `rdma-io/src/wc.rs` | `WorkCompletion`, `WcOpcode` (recv opcode discrimination) |
| `rdma-io/src/device.rs` | `any_device_is_iwarp()`, `supports_mw_type2(&pd)` detection |
| `rdma-io/src/async_cm.rs` | `AsyncCmListener` with `pending_requests` stash |
| `rdma-io-tests/tests/ring_transport_tests.rs` | 19 ring-specific integration tests (incl. MW probe) |
| `rdma-io-tests/tests/async_stream_tests.rs` | 22 generic stream tests (11 × 2 transports) |

## Future Work

- **`ReadRingTransport` sibling.** A sibling transport using RDMA Read for flow control
  (msquic-style) will share ~80% of the ring infrastructure. See
  [rdma-read-ring-transport.md](rdma-read-ring-transport.md) for the full design.
- **Credit keepalive**: Periodic 0-byte probe to detect hung peers.
- **Credit batching**: Send credit updates every N reposts to reduce WR overhead.
- **Large ring mode**: Length-only immediate encoding + RDMA Read for >64KB rings.
- **MW Local Invalidation**: Post `IBV_WR_LOCAL_INV` on disconnect to explicitly revoke
  the peer's write access before QP teardown (currently relies on `ibv_dealloc_mw` + QP destroy).
