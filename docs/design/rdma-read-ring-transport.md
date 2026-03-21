# RDMA Read Ring Transport

**Status:** Implemented  
**Date:** 2026-03-20  
**Implementation:** `rdma-io/src/read_ring_transport.rs` (~1200 lines)  
**Shared infrastructure:** `rdma-io/src/ring_common.rs` (~360 lines)  
**Reference:** [rdma-credit-ring-transport.md](rdma-credit-ring-transport.md) — CreditRingTransport (sibling)  
**Reference:** [rdma-transport-layer.md](rdma-transport-layer.md) — Transport architecture  
**Reference:** [RingPerformance.md](../future/RingPerformance.md) — Performance roadmap

## Overview

`ReadRingTransport` is a sibling to `CreditRingTransport` — both use RDMA Write + Immediate
Data over shared ring buffers but differ in **flow control**. Where Credit Ring pushes credit
updates via Send+Imm WRs, Read Ring uses RDMA Read to poll the receiver's head position
on demand, following the [msquic approach](../background/msquic-rdma.md).

```rust
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::transport::TransportBuilder;

let transport = ReadRingConfig::default().connect(&addr).await?;
let stream = AsyncRdmaStream::new(transport);
```

**Best for:** Low-utilization rings, CPU-sensitive workloads, bulk transfer.  
**Not for:** iWARP/siw, congested rings (>70% utilization), fan-in patterns.

## How It Differs from Credit Ring

| Aspect | CreditRingTransport | ReadRingTransport |
|--------|--------------------|--------------------|
| `repost_recv` | Post Send+Imm (credit WR) | Write `AtomicU32` to offset buffer (~1ns) |
| Sender space check | Compare `remote_credits` | Compare `cached_remote_head` vs write tail |
| On backpressure | Wait for credit recv completion | Post RDMA Read of remote offset buffer |
| Receiver Send WRs/msg | 1 (credit) | **0** |
| Memory Windows | 1 (recv ring, REMOTE_WRITE) | 2 (recv ring REMOTE_WRITE + offset buf REMOTE_READ) |
| Token version | v1 (20 bytes) | v2 (32 bytes) — includes offset buffer VA + rkey |
| Send CQ depth | `max_outstanding + 2` | `max_outstanding + 3` (extra for RDMA Read) |

Both transports share ~80% of their code via `ring_common.rs`: ring buffers, token exchange,
MW binding, imm encoding, virtual buffer index mapping, CompletionTracker, doorbell recv.

### Memory Layout

```
  Memory Region (single contiguous allocation):
  ┌────────────────────┬────────────────────┬─────────┬──────┐
  │   Send Ring        │   Recv Ring        │  pad    │ OBuf │
  │   (64 KB)          │   (64 KB)          │ (to 64B)│ (64B)│
  └────────────────────┴────────────────────┴─────────┴──────┘
                       ▲                              ▲
                       MW1 (REMOTE_WRITE)             MW2 (REMOTE_READ)
```

### Flow Control Sequence

```
  Sender                              Receiver
    │                                   │
    │  Check: cached_head vs tail       │
    │  Not enough space!                │
    │                                   │
    │ ── RDMA Read (4 bytes) ─────────►│  (passive — NIC serves the read)
    │ ◄── Read completion ─────────────│
    │  Update cached_head               │
    │  Now have space → send            │
    │                                   │
    │ ── Write+Imm(data) ────────────►│
    │                                   │  poll_recv → read data
    │                                   │  repost_recv:
    │                                   │    offset_buffer = new_head  (AtomicU32 store)
    │                                   │    (no WR posted)
```

### Hot-Path Architecture

```
  Sender (hot path)                       Receiver (hot path)
    │                                       │
    │  send_copy(data):                     │  poll_recv():
    │    1. Check cached_remote_head        │    1. ibv_poll_cq (doorbell CQ)
    │       vs write_tail → free?           │    2. Decode imm → (offset, len)
    │    2. If not: RDMA Read offset buf    │    3. Return RecvCompletion
    │       (rare — only on backpressure)   │
    │    3. memcpy → send_ring              │  repost_recv():
    │    4. ibv_post_send(WRITE_WITH_IMM)   │    1. CompletionTracker chase-forward
    │                                       │    2. Repost doorbell recv WR
    │  poll_send_completion():              │    3. AtomicU32::store(head, Release)
    │    Process Write completions          │       (NO ibv_post_send!)
    │    Silently handle Read completions   │
    │                                       │
```

## Key Design Decisions

### Dual Memory Windows

MW1 (`IBV_ACCESS_REMOTE_WRITE`) scopes the recv ring. MW2 (`IBV_ACCESS_REMOTE_READ`) scopes
the 4-byte offset buffer. Separate rkeys ensure the sender can only write to the ring and
read the offset buffer — never the reverse. A dedicated `bind_offset_mw` helper prevents
copy-paste errors from the existing `bind_recv_mw` (which hardcodes REMOTE_WRITE).

### Non-Blocking RDMA Read

`send_copy` is synchronous but RDMA Read is async. Solution: post a non-blocking Read,
return `Ok(0)`, pick up the result on the next `send_copy` call via `read_in_flight` flag.
Proactive Reads fire when free space drops below `2 * min_free_threshold`.

Read completions arrive on the **send CQ** (initiator operation). They are processed
silently — they update `cached_remote_head` but do NOT count as "≥1 send completed" in
`poll_send_completion` (prevents spurious wakeups in `AsyncRdmaStream::poll_write`).

### Out-of-Order Buffer Release

The `Transport` trait allows `repost_recv` in any order. Since the sender reads the ring
head position from the offset buffer, the receiver must only advance the head through
**contiguous** released slots. `CompletionTracker` (bitmap chase-forward) handles this,
with `slot_lengths: Box<[usize]>` mapping slot releases to byte positions for variable-size
messages. This same mechanism was retrofitted to CreditRingTransport to fix the
[OOO overwrite bug](../bugs/credit-ring-ooo-overwrite.md).

### Offset Buffer Atomicity

The receiver writes `AtomicU32::store(head, Ordering::Release)` for portability across
architectures (x86, ARM64). The offset buffer is 64-byte aligned to avoid false sharing
with the recv ring.

### Wrap-Aware Free Space

Free space uses explicit if/else (not `wrapping_sub`, which gives wrong results when
`head < tail` with positions in `[0, capacity)`):

```rust
let free = if cached_head >= write_tail {
    cached_head - write_tail
} else {
    remote_capacity - write_tail + cached_head
};
```

### Padding Through CompletionTracker

Padding (zero-length Write+Imm for ring wrap) is assigned a CompletionTracker arrival-seq
slot and immediately released — keeping the tracker synchronized with the ring head.

## When to Use Each Transport

| Scenario | Best Transport | Why |
|----------|---------------|-----|
| Congested ring (>70%) | **Credit Ring** | Proactive push, no polling |
| Backpressure-heavy | **Credit Ring** | Zero-latency recovery |
| Fan-in (many senders) | **Credit Ring** | O(1) push vs O(N) Reads |
| Low-moderate load | **Read Ring** | Zero receiver WRs |
| Bulk transfer, few stalls | **Read Ring** | Reads rare when ring rarely fills |

Crossover at ~70-84% utilization (conservative bound: 70%). Exact point depends on
ring capacity, message size, and credit batch size N.

### Ring Sizing for High Throughput

Default 64KB ring holds ~54 × 1200B messages = 10.8µs buffer at 5M msg/sec.

| Target rate | Ring capacity | Mode |
|-------------|---------------|------|
| 1M msg/sec | 64 KB (default) | Small ring |
| 5M msg/sec | ~600 KB | Large ring (future) |
| 10M msg/sec | ~1.2 MB | Large ring (future) |

## Performance Comparison

| Design | Sender post_send | Receiver post_send | Receiver post_recv | Total doorbells |
|--------|-----------------|-------------------|-------------------|----------------|
| Credit Ring (current) | 1M | 1M | 1M | 4M |
| Credit Ring (batched N=8) | 1M | 125K | 1M | 3.125M |
| Read Ring + Sel. Signal | 1M | 0 | 1M | **2.125M** |

Receiver CPU per message (estimates — benchmark on target NIC):

| Transport | Recv CPU/msg | Savings vs unbatched |
|-----------|-------------|---------------------|
| Credit Ring (unbatched) | ~620ns | baseline |
| Credit Ring (batched N=8) | ~271ns | 56% |
| Read Ring + Sel. Signal | ~221ns | 64% |

Note: Selective signaling is not yet implemented — these numbers show the fully
optimized target. Without it, Read Ring's advantage is primarily on the receiver
side (zero Send WRs).

## Files

| File | Description |
|------|-------------|
| `rdma-io/src/read_ring_transport.rs` | ReadRingTransport (~1200 lines) |
| `rdma-io/src/ring_common.rs` | Shared ring infrastructure (~360 lines) |
| `rdma-io/src/credit_ring_transport.rs` | CreditRingTransport, refactored (~1000 lines) |
| `rdma-io-tests/tests/read_ring_transport_tests.rs` | 8 transport-level tests |
| `rdma-io-tests/tests/async_stream_tests.rs` | 11 stream tests (33 total) |
| `rdma-io-tests/tests/quinn_tests.rs` | 2 Quinn integration tests (6 total) |
| `rdma-io-tests/tests/tonic_tests.rs` | 4 tonic gRPC tests (12 total) |
| `rdma-io-tests/tests/tonic_tls_tests.rs` | 1 mTLS test (3 total) |
| `rdma-io-tests/tests/tonic_h3_tests.rs` | 4 H3/QUIC tests (12 total) |

## msquic Comparison

Aligned: dual MWs, offset buffer RDMA Read, dual CQ, imm encoding, token exchange,
min free threshold, out-of-order recv release (chase-forward).

Divergences: no send-side hash table (RC QPs guarantee in-order), `Box<[bool]>` bitmap
instead of hash table, `Ok(0)` backpressure instead of internal send queue, `AtomicU32`
offset buffer for portability.

See [msquic-rdma.md](../background/msquic-rdma.md) for the full background analysis.

## Future Work

- **Large ring mode (>64KB)** — length-only imm encoding, receiver tracks tail from
  sequential RC completions. Shared between both ring transports. (Phase 3)
- **MW invalidation on disconnect** — post `IBV_WR_LOCAL_INV` for MW2 then MW1 before
  QP teardown. Applies to both ring transports.
- **Selective signaling** — signal every Nth WR to reduce send CQ overhead. Shared.
- **Inline small messages** — wire `max_inline_data` config into send path. Shared.
- **Adaptive flow control** — auto-switch between Credit and Read based on utilization. (P3)
- **Read coalescing** — read 64B instead of 4B for free telemetry counters. (P3)
- **No-doorbell polling** — eliminate doorbell recv; spin-poll sequence counter. (P4)
