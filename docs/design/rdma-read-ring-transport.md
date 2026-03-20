# RDMA Read Ring Transport

**Status:** Future — Design complete, not yet implemented  
**Date:** 2026-03-20  
**Prerequisite:** [rdma-credit-ring-transport.md](rdma-credit-ring-transport.md) — CreditRingTransport (implemented)  
**Reference:** [rdma-transport-layer.md](rdma-transport-layer.md) — Transport architecture  
**Reference:** [rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way comparison  
**Reference:** [RingPerformance.md](../future/RingPerformance.md) — Performance roadmap & benchmarking

## Overview

`ReadRingTransport` is a sibling to `CreditRingTransport` — both use RDMA Write + Immediate
Data over shared ring buffers but differ in **flow control**. Where Credit Ring pushes credit
updates via Send+Imm WRs, Read Ring uses RDMA Read to poll the receiver's head position
on demand, following the [msquic approach](../background/msquic-rdma.md).

```rust
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::transport::TransportBuilder;

// Same interface — different flow control engine
let transport = ReadRingConfig::default().connect(&addr).await?;
let stream = AsyncRdmaStream::new(transport);
```

**Best for:** HFT (>5M msg/sec), low-utilization rings, CPU-sensitive workloads.  
**Not for:** iWARP/siw, congested rings (>70% utilization), fan-in patterns.

## Transport Family Context

Both ring transports share ~80% of their code: ring buffers, token exchange, doorbell recv,
imm encoding, MW binding, virtual buffer index mapping. They differ only in `repost_recv`
(receiver side) and the sender's space-check path.

| Transport | Flow Control | Status |
|-----------|-------------|--------|
| `CreditRingTransport` | Receiver pushes credits (SendWithImm) | Implemented |
| `ReadRingTransport` | Sender polls remote head (RDMA Read) | This doc |

See [rdma-transport-layer.md](rdma-transport-layer.md#transport-naming-convention) for the
full naming convention.

## Architecture

### Data Path (Shared with Credit Ring)

| Operation | ReadRingTransport |
|-----------|-------------------|
| `send_copy()` | Check cached head vs tail → copy → `post_send(WRITE_WITH_IMM)` |
| `poll_recv()` | Poll RecvCQ for doorbells, decode imm → virtual `RecvCompletion` |
| `recv_buf(idx)` | `recv_ring[offset..offset+len]` via `virtual_idx_map` |
| Backpressure | `Ok(0)` when cached head shows ring full |
| Setup | Create QP, alloc rings, exchange tokens (VA + rkey + capacity + offset buf) |

### Flow Control (Different from Credit Ring)

| Operation | CreditRingTransport | ReadRingTransport |
|-----------|--------------------|--------------------|
| `repost_recv(idx)` | Advance head, repost doorbell, **post Send+Imm** (credit WR) | Advance head, repost doorbell, **write u32 to offset buffer** (~1ns) |
| Sender space check | Compare `remote_credits` (pushed by receiver) | Compare `cached_remote_head` vs write tail |
| On backpressure | Wait for credit recv completion | **Post RDMA Read** of remote offset buffer → update cache |
| Receiver WRs/msg | 1 Send+Imm (credit) + 1 post_recv (doorbell) | **0 Send WRs** + 1 post_recv (doorbell) |

## Design

1. Each side allocates a 4-byte "offset buffer" MR containing the current ring head
2. Each side binds **two Memory Windows**: one for the recv ring, one for the offset buffer
   (separate rkeys — minimum attack surface, follows msquic's dual-MW pattern)
3. Token exchange includes both MW rkeys + VAs (see Token Format below)
4. Sender tracks `cached_remote_head` locally
5. Before sending: `free_space = cached_remote_head - write_tail` (with wrap)
6. If free space < payload + `min_free_threshold`: post `RDMA Read` of remote offset buffer → update cache
7. Receiver: after consuming data, writes new head to local offset buffer (no WR)

### Token Format

The token exchanged during connection setup grows from 20 → 32 bytes to include
offset buffer metadata:

```
ReadRingToken (32 bytes):
  ring_va:       u64   // Base VA of recv ring buffer
  ring_capacity: u32   // Ring buffer size in bytes
  ring_rkey:     u32   // MW rkey scoped to recv ring
  offset_va:     u64   // VA of 4-byte offset buffer (contains recv head)
  offset_rkey:   u32   // MW rkey scoped to offset buffer
  _reserved:     u32   // Alignment / future use
```

msquic's `RDMA_DATAPATH_PRIVATE_DATA` is 28 bytes with the same five fields
(no reserved). We add 4 bytes for alignment.

### Memory Window Layout

```
  Memory Region (single contiguous allocation):
  ┌────────────────────┬────────────────────┬──────┐
  │   Send Ring        │   Recv Ring        │ OBuf │
  │   (64 KB)          │   (64 KB)          │ (4B) │
  └────────────────────┴────────────────────┴──────┘
                       ▲                    ▲
                       MW1 (recv ring)      MW2 (offset buffer)

  MW1 rkey → scoped to recv ring only (peer writes data here)
  MW2 rkey → scoped to offset buffer only (peer reads head position here)
```

Two separate Memory Windows ensure the sender can only write to the recv ring
and can only read the offset buffer. Neither MW exposes the send ring or other
memory. This matches msquic's `RecvMemoryWindow` + `OffsetMemoryWindow` pattern.

### Minimum Free Space Threshold

When the ring has very little free space, the sender could enter a tight RDMA Read
loop. A `min_free_threshold` (default: 128 bytes, matching msquic's
`MIN_FREE_BUFFER_THRESHOLD`) prevents this:

- If `free_space < payload + min_free_threshold`: treat as backpressure, post RDMA Read
- If `free_space >= payload + min_free_threshold` but `< payload + 2 * min_free_threshold`:
  proactively post a non-blocking RDMA Read to refresh the cache for the next send

This avoids the degenerate case where every send triggers a blocking Read.

### Sequence Diagram

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
    │                                   │    offset_buffer = new_head  ← local write only!
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
    │    4. ibv_post_send(WRITE_WITH_IMM)   │    1. Advance recv_ring.head
    │       INLINE if ≤ max_inline_data     │    2. Repost doorbell recv WR
    │       UNSIGNALED (every N signaled)   │    3. *offset_buffer = head  ← u32 store
    │                                       │       (NO ibv_post_send!)
    │  poll_send_completion():              │
    │    Only 1/N completions to process    │
    │    Each releases N WRs of ring space  │
    │                                       │
```

## Race Condition Analysis

```
  Time   Sender                     Receiver
  t0     Read offset_buf → head=100
  t1                                Write offset_buf = 200  (consumed 100 msgs)
  t2     Use cached head=100        (stale!)
  t3     Think only 100 free        Actually 200 free
```

This is safe but suboptimal — the sender underestimates free space, which just
means it does another Read sooner than necessary. No correctness issue.

The dangerous race (sender sees head AHEAD of actual) can't happen because the
receiver only advances the head, never moves it backward.

## Out-of-Order Buffer Release

### The Problem

The `Transport` trait allows the application to call `repost_recv(buf_idx)` in any
order — not necessarily the order that `poll_recv` returned the completions. For
example, QUIC may process control packets before data packets.

In CreditRingTransport, this is safe: credits are an **absolute count**, not a
position. Releasing buf_idx B before buf_idx A sends credit value `2` (two freed),
and the sender correctly computes `delta = 2 - 0 = 2`. The ring head position
doesn't matter for credit flow control.

In ReadRingTransport, the sender reads the **ring head position** from the offset
buffer. If the receiver advances the head past a gap (unreleased buffer), the
sender would see stale or incorrect free space:

```
  Ring: [A=1500B][B=1500B][C=1500B][  free  ]
  Head=0                            Tail=4500

  App calls repost_recv(B) before repost_recv(A):
    ✗ WRONG: head = 1500 + 1500 = 3000  (skipped A, sender thinks 0..3000 is free)
    ✓ RIGHT: head = 0                     (A not released, head can't advance past it)
```

### Solution: Chase-Forward Release

The receiver uses a **completion tracker** to handle out-of-order releases.
Each ring slot is marked as released independently; the head only advances
through contiguous released slots:

```rust
// Existing CompletionTracker in credit_ring_transport.rs (currently dead_code)
fn repost_recv(&mut self, buf_idx: usize) -> Result<()> {
    let (offset, length) = self.virtual_idx_map[buf_idx].take().unwrap();

    // Mark this slot as released in the tracker.
    self.completion_tracker.mark_released(offset, length);

    // Chase forward: advance head through all contiguous released slots.
    let new_head = self.completion_tracker.chase_head();
    self.recv_ring.head = new_head;

    // Update offset buffer — sender sees contiguous head only.
    *self.offset_buffer = new_head as u32;

    // Repost doorbell recv WR.
    self.repost_doorbell()?;
    Ok(())
}
```

```
  Example: recv_ring with 3 messages (A at 0, B at 1500, C at 3000):

  repost_recv(B):  tracker: [_][✓][_]  → head stays at 0 (A blocks)
                   offset_buffer = 0

  repost_recv(A):  tracker: [✓][✓][_]  → chase: head advances past A+B
                   head = 3000, offset_buffer = 3000

  repost_recv(C):  tracker: [_][_][✓]  → chase: head advances past C
                   head = 4500, offset_buffer = 4500
```

### msquic Approach

msquic solves the same problem with a hash table (`RecvCompletionTable`) in
`RdmaLocalReceiveRingBufferRelease`:

```c
if (Offset == RecvRingBuffer->Head) {
    // In-order: advance Head, then chase through hash table
    RecvRingBuffer->Head += Length;
    while (HashLookup(Head) != NULL) {
        RecvRingBuffer->Head += stashed_entry.Length;
    }
} else {
    // Out-of-order: stash in hash table for later chasing
    HashInsert(Offset, Length);
}
```

Our `CompletionTracker` (already in `credit_ring_transport.rs:238`, currently
`#[allow(dead_code)]`) uses a `Vec<bool>` bitmap instead of a hash table —
simpler and faster for bounded slot counts. The chase-forward logic is identical.

### Impact on CreditRingTransport

CreditRingTransport does NOT need chase-forward because:
- Credits are an absolute count (position-independent)
- `recv_ring.release(length)` advances head, but the head position is never
  read by the sender — only the credit count matters
- Out-of-order `repost_recv` sends credit updates in any order; the absolute
  encoding self-corrects

If CreditRingTransport is later extended to expose head position (e.g., for
diagnostics or large ring mode), chase-forward would need to be added.

## Padding and Offset Buffer Interaction

When ring wrap-around occurs, the sender writes a zero-length padding Write+Imm
to fill the gap at the end of the ring, then writes the actual data at offset 0.
The receiver must handle padding correctly with offset buffer updates:

```
  Receiver sees padding (imm length == 0):
    1. Compute pad_len = ring_capacity - offset
    2. Advance recv_ring.head past padding (head += pad_len, or head = 0)
    3. Repost doorbell recv WR
    4. Write updated head to offset_buffer  ← CRITICAL for sender visibility
       (without this, sender's RDMA Read sees stale head behind the wrap point)
```

In CreditRingTransport, padding triggers a credit update (Send+Imm) which
explicitly notifies the sender. In ReadRingTransport, the offset buffer update
is the ONLY mechanism — if the receiver forgets to update the offset buffer
after padding, the sender will repeatedly Read a stale head and stall.

**Implementation note:** Both data release and padding release must write to
the offset buffer. The `repost_recv` path handles data; a separate
`release_padding` path (called from `poll_recv` or `drain_recv_credits`)
handles padding. Both paths must end with `*offset_buffer = recv_ring.head`.

## Large Ring Mode (>64KB)

The current 16-bit immediate data encoding (`offset<<16 | length`) limits rings
to 64KB. For larger rings, the encoding changes to length-only:

```
Small ring (≤ 64KB):  imm = (offset << 16) | length    // 16-bit each
Large ring (> 64KB):  imm = length                      // 32-bit length
```

In large ring mode, the receiver cannot decode the write offset from the
immediate data alone. Instead, the receiver **tracks its own recv_ring.tail**
which advances sequentially as completions arrive (RC QP guarantees in-order
delivery). The offset is implicitly `recv_ring.tail` at the time each
completion is processed.

```
  Large ring recv:
    1. Doorbell CQ fires with imm = length
    2. offset = recv_ring.tail   (receiver tracks this locally)
    3. recv_ring.tail += length  (advance past data)
    4. Handle wrap: if tail + length > capacity, the preceding completion
       was padding (length == 0), which advanced tail to 0
```

**Offset buffer serves double duty in large ring mode:**
- **Flow control:** Sender reads remote offset buffer → receiver's head (same as small ring)
- **Position tracking:** Not needed — receiver tracks tail locally from sequential completions

This means large ring mode does NOT require any additional offset buffers or
RDMA Reads beyond what ReadRingTransport already uses for flow control. The
key insight is that RC QP ordering makes the receiver's tail position
deterministic from the completion sequence.

**Config:** Add `large_ring: bool` flag to `ReadRingConfig` (auto-enabled when
`ring_capacity > 65536`). Both ring transports can benefit from this encoding.

## When to Use Each Transport

| Scenario | Best Transport | Why |
|----------|---------------|-----|
| Congested ring (>70% utilization) | **Credit Ring** | Proactive push — sender learns of space immediately |
| Backpressure-heavy workloads | **Credit Ring** | Zero-latency recovery — credit arrives, no polling needed |
| Fan-in (many senders → one receiver) | **Credit Ring** | O(1) push per sender vs. O(N) RDMA Reads on receiver NIC |
| Low-moderate load, CPU-sensitive | **Read Ring** | Zero receiver WRs — offset buffer write is ~1ns |
| HFT / >5M msg/sec | **Read Ring** | Minimal NIC doorbells — 1M doorbells for 1M messages |
| Bulk transfer (large ring, few stalls) | **Read Ring** | Reads are rare when ring rarely fills |

### Crossover Model

```
Credit batch WRs = send_rate / N   (e.g., 125K/s at 1M msg/s, N=8)
RDMA Read WRs   ≈ send_rate × f(ring_utilization)

Crossover: at ~70% ring utilization with small rings, Read frequency
exceeds credit batch frequency. Above this, Credit Ring has fewer total
WRs AND avoids RTT stalls on the sender hot path.
```

## Performance Comparison

### Per-Message Overhead

| Aspect | Credit Ring (batched, N=8) | Read Ring |
|--------|--------------------------|-----------|
| Receiver WRs per msg | 1/N | **0** |
| Sender WRs per msg | 1 | 1 (+ occasional Read) |
| Total WRs at 1M msg/s | 1.125M | ~1.001M |
| Backpressure recovery | **Immediate** (credit pushed) | RTT delay (must Read) |
| Fan-in (N senders) | **O(1) push** per sender | O(N) Reads on receiver NIC |
| Near-full ring (>70%) | **Proactive** — no stalls | Frequent Reads + RTT stalls |
| Low-load ring (<30%) | Credit WRs wasted | **Reads near-zero** |
| Self-healing on loss | ✅ (absolute encoding) | ✅ (Read always gets latest) |

### Cost Per 1M Messages/sec

| Design | Sender `ibv_post_send` | Sender CQ entries | Receiver `ibv_post_send` | Receiver CQ entries | Total NIC doorbells |
|--------|----------------------|-------------------|------------------------|--------------------|-------------------|
| **Credit Ring (current)** | 1M | 1M | 1M (credits) | 1M (credits) | 3M |
| **Credit Ring (batched N=8)** | 1M | 1M | 125K | 125K | 2.125M |
| **Read Ring + Sel. Signal** | 1M | 125K (N=8) | **0** | **0** | **1M** |

The Read Ring design achieves **1M NIC doorbells for 1M messages** — the
theoretical minimum (one post_send per message, everything else is either
local memory or NIC-served).

### Receiver CPU Breakdown

```
                    Credit Ring     Credit Ring        Read Ring
                    (current)       (batched N=8)      + Sel. Signal
                    ┌─────────┐     ┌─────────┐      ┌─────────┐
  ibv_post_send     │ ████████│     │ █       │      │         │  ← eliminated
  (credit WR)       │ ~300ns  │     │ ~38ns   │      │   0ns   │
                    ├─────────┤     ├─────────┤      ├─────────┤
  ibv_poll_cq       │ ████    │     │ ████    │      │ ████    │  ← same
  (credit CQ)       │ ~100ns  │     │ ~13ns   │      │   0ns   │
                    ├─────────┤     ├─────────┤      ├─────────┤
  ibv_poll_cq       │ ████    │     │ ████    │      │ ████    │  ← same
  (doorbell CQ)     │ ~100ns  │     │ ~100ns  │      │ ~100ns  │
                    ├─────────┤     ├─────────┤      ├─────────┤
  repost doorbell   │ ██      │     │ ██      │      │ ██      │  ← same
  (ibv_post_recv)   │ ~50ns   │     │ ~50ns   │      │ ~50ns   │
                    ├─────────┤     ├─────────┤      ├─────────┤
  offset buf write  │         │     │         │      │ ▏       │  ← ~1ns
                    │         │     │         │      │         │
                    └─────────┘     └─────────┘      └─────────┘
  Total recv CPU:    ~550ns          ~200ns            ~151ns

  Savings vs current:  —             63%               73%
```

### Transport Tier Summary

| Tier | Transport | Recv CPU/msg | Use case |
|------|----------|-------------|----------|
| **Simple** | Credit Ring (unbatched) | ~550ns | Development, correctness testing |
| **Balanced** | Credit Ring (batched N=8) | ~200ns | Production gRPC, congested rings, fan-in |
| **Lowest-CPU** | Read Ring + Sel. Signal + Inline | ~151ns | HFT, >5M msg/sec, low-utilization rings |

## Implementation Plan

### New Components

| Component | Effort |
|-----------|--------|
| `ReadRingTransport` struct | Small — mirrors Credit Ring, minus credit fields, plus offset buffer |
| Offset buffer MR (per side) | Small — 4-byte MR, register in single contiguous allocation |
| **Second Memory Window** (offset buffer) | Small — bind MW2 scoped to 4-byte offset buffer |
| Token expansion (20 → 32 bytes) | Small — add `offset_va` + `offset_rkey` + reserved fields |
| `ReadRingConfig` + `TransportBuilder` impl | Small — mirrors `CreditRingConfig`, adds `min_free_threshold` |
| RDMA Read WR posting | Medium — new `WrOpcode::RdmaRead` path, async completion wait |
| `repost_recv` (no credit WR) | Small — chase-forward release + **write offset buffer** + repost doorbell |
| **CompletionTracker** (chase-forward) | Small — activate existing `CompletionTracker` for out-of-order release |
| **Padding release** (offset buffer update) | Small — advance head past padding + write offset buffer |
| `send_copy` space check (Read path) | Medium — cache remote head, RDMA Read on backpressure |
| **Min free threshold** logic | Small — proactive Read when free space is low |

**Total: ~350 new lines for Read Ring + ~200 lines refactored into shared module.**

### Shared Code (Extract from CreditRingTransport)

The following modules are shared between both ring transports (~500 lines to extract):

- Ring buffer allocation and management
- Immediate data encoding/decoding (offset + length)
- Token exchange protocol (VA, rkey, capacity)
- Memory Window binding and validation
- Virtual buffer index mapping (`virtual_idx_map`)
- CompletionTracker (chase-forward out-of-order release — used by ReadRing, available to CreditRing)
- Doorbell recv posting and CQ polling
- Send path (Write+Imm WR construction)

### Config

```rust
pub struct ReadRingConfig {
    pub ring_capacity: usize,         // default: 65536 (64 KB)
    pub max_message_size: usize,      // default: 1500 (MTU)
    pub token_timeout: Duration,      // default: 5s
    pub max_inline_data: u32,         // default: 0
    pub min_free_threshold: usize,    // default: 128 (bytes, matches msquic)
}
// large_ring mode auto-enabled when ring_capacity > 65536
```

### Implementation Phases

```
  Phase 1: Extract shared ring infrastructure (~500 lines)
    └── Ring buffer, token exchange, MW binding, imm encoding, virtual idx map
    └── Activate CompletionTracker (move from dead_code to ring_common)
    └── CreditRingTransport becomes a thin wrapper around shared + credit-specific code

  Phase 2: Implement ReadRingTransport (~350 lines)
    └── Contiguous MR allocation (send ring + recv ring + 4-byte offset buffer)
    └── Second MW bind for offset buffer (MW2)
    └── Token expansion (20 → 32 bytes) with offset_va + offset_rkey
    └── repost_recv: CompletionTracker chase-forward + write offset buffer (no Send WR)
    └── Padding release: advance head past padding + write offset buffer
    └── send_copy: cached_remote_head check + RDMA Read on backpressure
    └── Min free threshold: proactive non-blocking Read when space is low
    └── ReadRingConfig + TransportBuilder impl

  Phase 3: Large ring mode (shared, optional)
    └── Length-only imm encoding when ring_capacity > 64KB
    └── Receiver tracks recv_ring.tail from sequential completions
    └── Applies to both CreditRingTransport and ReadRingTransport

  Phase 4: Tests + integration
    └── Mirror ring_transport_tests.rs for ReadRingTransport
    └── Out-of-order repost_recv test (verify offset buffer reflects chase-forward head)
    └── Add to async_stream_tests.rs generic transport matrix
    └── Quinn integration test (ReadRingConfig as TransportBuilder)
```

## Files (Planned)

| File | Description |
|------|-------------|
| `rdma-io/src/read_ring_transport.rs` | ReadRingTransport implementation (~300 lines) |
| `rdma-io/src/ring_common.rs` | Shared ring infrastructure (~500 lines, extracted) |
| `rdma-io/src/credit_ring_transport.rs` | Refactored to use `ring_common` |
| `rdma-io/src/transport.rs` | `Transport` + `TransportBuilder` traits (unchanged) |
| `rdma-io-tests/tests/read_ring_transport_tests.rs` | Read Ring integration tests |

## Design Notes — msquic Comparison

This design was informed by [msquic PR #5113](../background/msquic-rdma.md). Key
alignments and intentional divergences:

### Aligned with msquic

| Aspect | msquic | ReadRingTransport |
|--------|--------|-------------------|
| Dual Memory Windows | `RecvMemoryWindow` + `OffsetMemoryWindow` | MW1 (recv ring) + MW2 (offset buffer) |
| Offset buffer for flow control | Sender reads remote offset buffer via RDMA Read | Same |
| Dual CQ | Separate send/recv CQs | Same (shared with CreditRingTransport) |
| Imm encoding (small ring) | `offset<<16 \| length` (16-bit each) | Same |
| Token exchange | Post-connect Send/Recv of ring metadata | Same |
| Min free threshold | `MIN_FREE_BUFFER_THRESHOLD = 128` bytes | `min_free_threshold` config field |
| Out-of-order recv release | `RecvCompletionTable` hash + chase-forward | `CompletionTracker` bitmap + chase-forward |

### Intentional Divergences

| Aspect | msquic | ReadRingTransport | Rationale |
|--------|--------|-------------------|-----------|
| Out-of-order **send** completion | Hash tables (`SendCompletionTable`) | Sequential ring advance via wr_id | RC QPs on ibverbs guarantee in-order send CQ completions; NDSPI may reorder |
| Recv release data structure | Hash table (unbounded) | `Vec<bool>` bitmap (bounded by `max_outstanding`) | Bounded slot count makes bitmap simpler and faster |
| Backpressure handling | `SendQueue` queues pending sends | Returns `Ok(0)` — caller retries | Matches our `Transport` trait contract; avoids internal queue complexity |
| MR allocation | Single contiguous MR for all buffers | Same approach planned | Single MR simplifies registration |
| Offset buffer conditional | Only for rings > 64KB | Always present | We always need it for flow control (not just position tracking) |
| Large ring mode | imm = length-only, receiver tracks tail | Same approach (Phase 3) | RC QP ordering makes receiver tail deterministic |
| Platform | Windows NDSPI / MANA extensions | Linux ibverbs / rdma_cm | Portable across IB, RoCE, (not iWARP) |

### Not Applicable to Our Design

- **13-state connection machine**: msquic's complexity comes from IOCP-style async connect + accept.
  Our rdma_cm provides a simpler blocking connect/accept with token exchange as a post-connect step.
- **MANA-specific CQ extensions**: `IND2ManaCompletionQueue::GetManaResults()` returns
  `ND2_MANA_RESULT` with request type discrimination. ibverbs `WorkCompletion` provides
  equivalent via `opcode()` + `wc_flags()`.
- **12 IOCP event handlers**: Our async model uses `poll_fn` + tokio `AsyncFd` — one code path
  per CQ, not per event type.

## Priority

**P2** — implement after `CreditRingTransport` is production-validated and credit batching
(P0) + inline small messages (P1) are done. See [RingPerformance.md](../future/RingPerformance.md)
for the full priority roadmap.
