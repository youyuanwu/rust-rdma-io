# RDMA Read Ring Transport

**Status:** Implemented  
**Date:** 2026-03-20  
**Implementation:** `rdma-io/src/read_ring_transport.rs`  
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
2. Each side binds **two Memory Windows**: one for the recv ring (`IBV_ACCESS_REMOTE_WRITE`),
   one for the offset buffer (`IBV_ACCESS_REMOTE_READ` **only** — NOT REMOTE_WRITE).
   Separate rkeys, minimum attack surface, follows msquic's dual-MW pattern.
   **CRITICAL**: Use a dedicated `bind_offset_mw` helper, not the existing `bind_recv_mw`
   (which hardcodes REMOTE_WRITE). A forged head value from a malicious peer would
   cause the sender to overwrite in-use recv ring data.
3. Token exchange includes both MW rkeys + VAs (see Token Format below)
4. Sender tracks `cached_remote_head` locally
5. Before sending: compute free space with wrap-aware arithmetic (NOT `wrapping_sub` —
   see Free Space Calculation below)
6. If free space < payload + `min_free_threshold`: post `RDMA Read` of remote offset buffer → update cache
7. Receiver: after consuming data, writes new head to local offset buffer via
   `AtomicU32::store(head, Ordering::Release)` for portability across architectures

### Token Format

The token exchanged during connection setup grows from 20 → 32 bytes to include
offset buffer metadata:

```
ReadRingToken (32 bytes):
  version:       u8    // = 2 (CreditRingToken is version 1)
  _reserved:     [u8; 3] // Alignment padding
  ring_va:       u64   // Base VA of recv ring buffer
  ring_capacity: u32   // Ring buffer size in bytes
  ring_rkey:     u32   // MW rkey scoped to recv ring (REMOTE_WRITE)
  offset_va:     u64   // VA of 4-byte offset buffer (contains recv head)
  offset_rkey:   u32   // MW rkey scoped to offset buffer (REMOTE_READ)
```

The version byte allows future protocol detection (e.g., a generic `RingListener`
that accepts either transport type). CreditRingToken uses `version: 1` (20 bytes);
ReadRingToken uses `version: 2` (32 bytes). The shared `ring_common` module
should define a common token header.

### Memory Window Layout

```
  Memory Region (single contiguous allocation):
  ┌────────────────────┬────────────────────┬──────┐
  │   Send Ring        │   Recv Ring        │ OBuf │
  │   (64 KB)          │   (64 KB)          │ (4B) │
  └────────────────────┴────────────────────┴──────┘
                       ▲                    ▲
                       MW1 (recv ring)      MW2 (offset buffer)

  MW1 rkey → scoped to recv ring only (REMOTE_WRITE — peer writes data here)
  MW2 rkey → scoped to offset buffer only (REMOTE_READ — peer reads head here)
```

Two separate Memory Windows ensure the sender can only write to the recv ring
and can only read the offset buffer. Neither MW exposes the send ring or other
memory. This matches msquic's `RecvMemoryWindow` + `OffsetMemoryWindow` pattern.

**CQ depth**: The send CQ must accommodate RDMA Read completions alongside Write+Imm
completions. Formula: `send_cq_depth = max_outstanding + 3` (data WRs + 1 Read + 2 setup).
The `read_in_flight` flag ensures at most 1 concurrent Read WR.

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
fn repost_recv(&mut self, buf_idx: usize) -> Result<()> {
    let (offset, length, arrival_seq) = self.virtual_idx_map[buf_idx].take().unwrap();

    // Mark this arrival-seq slot as released in the tracker.
    let slot = arrival_seq % self.max_outstanding;
    let contiguous = self.completion_tracker.release(slot);

    // Chase forward: advance head by the byte lengths of contiguous released slots.
    // The tracker returns a slot COUNT; we need byte LENGTHS. Use an auxiliary
    // per-slot length array (Box<[usize]>) to map slot releases to byte offsets.
    for _ in 0..contiguous {
        let slot_len = self.slot_lengths[self.head_slot_idx];
        self.recv_ring.head = (self.recv_ring.head + slot_len) % self.recv_ring.capacity;
        self.head_slot_idx = (self.head_slot_idx + 1) % self.max_outstanding;
    }

    // Update offset buffer — sender sees contiguous head only.
    if contiguous > 0 {
        self.offset_buffer.store(self.recv_ring.head as u32, Ordering::Release);
    }

    // Repost doorbell recv WR.
    self.repost_doorbell()?;
    Ok(())
}
```

**Slot-to-position mapping**: The `CompletionTracker` returns a contiguous slot
**count**, not a byte position. For variable-length messages, an auxiliary
`slot_lengths: Box<[usize]>` stores the byte length of each arrival-seq slot.
When the tracker chases forward by N slots, the head advances by the sum of
those N slot lengths. This is set when messages arrive: `slot_lengths[seq % max_outstanding] = length`.

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

Our `CompletionTracker` (already activated in `credit_ring_transport.rs`)
uses a `Box<[bool]>` bitmap instead of a hash table —
simpler and faster for bounded slot counts. The chase-forward logic is identical.

### Impact on CreditRingTransport

CreditRingTransport **also uses chase-forward** since the out-of-order overwrite
bug fix (see [credit-ring-ooo-overwrite.md](../bugs/credit-ring-ooo-overwrite.md)).
Both ring transports use `CompletionTracker` for the same structural reason but
for different effects:

- **CreditRingTransport**: Chase-forward gates **credit updates** — credits are
  only sent when contiguous slots advance, preventing the sender from wrapping
  `remote_write_tail` into unreleased ring positions.
- **ReadRingTransport**: Chase-forward gates **offset buffer writes** — the head
  position only advances through contiguous released slots.

## Padding and Offset Buffer Interaction

When ring wrap-around occurs, the sender writes a zero-length padding Write+Imm
to fill the gap at the end of the ring, then writes the actual data at offset 0.
The receiver must handle padding correctly with offset buffer updates:

```
  Receiver sees padding (imm length == 0):
    1. Compute pad_len = ring_capacity - offset
    2. Assign a CompletionTracker arrival_seq slot (padding consumes a sender credit)
    3. Immediately release the slot in the tracker (padding is auto-consumed)
    4. If contiguous advance > 0: write updated head to offset_buffer
    5. Repost doorbell recv WR
```

**CRITICAL**: Padding MUST go through the CompletionTracker, not bypass it.
If the ring head advances without the tracker knowing, subsequent `repost_recv`
calls compute wrong offset buffer values (tracker's `head_slot` desyncs from
the ring's actual head). The CreditRingTransport implementation handles this
correctly — padding gets a tracker slot and is immediately released (see
`credit_ring_transport.rs` padding handling in `poll_recv` and `drain_recv_credits`).

In CreditRingTransport, padding triggers a credit update (Send+Imm) which
explicitly notifies the sender. In ReadRingTransport, the offset buffer update
is the ONLY mechanism — if the receiver forgets to update the offset buffer
after padding, the sender will repeatedly Read a stale head and stall.

**Implementation note:** Both data release and padding release must go through
the CompletionTracker. The `repost_recv` path handles data (mark slot, chase,
write offset buffer); the `release_padding` path (called from `poll_recv` or
`drain_recv_credits`) assigns a seq, immediately releases, and writes offset
buffer if contiguous advance > 0.

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
RDMA Read WRs   ≈ send_rate × msg_size / (ring_capacity × (1 - utilization))

Example (64KB ring, 1200B msgs, N=8):
  At 70% utilization: reads ≈ 1M × 1200 / (65536 × 0.3) ≈ 61K  (< 125K credits → Read wins)
  At 84% utilization: reads ≈ 1M × 1200 / (65536 × 0.16) ≈ 114K  (≈ 125K → crossover)
  At 90% utilization: reads ≈ 1M × 1200 / (65536 × 0.1) ≈ 183K  (> 125K → Credit wins)

Crossover shifts with ring size: 256KB ring → crossover at ~95%+ utilization.
```

**Note**: The crossover is workload-dependent. The guidance in the "When to Use" table
uses **70% as a conservative bound** — Read Ring is reliably better below this.
The exact crossover for a given deployment depends on ring capacity, message size,
and credit batch size N.

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

| Design | Sender `post_send` | Sender CQ entries | Receiver `post_send` | Receiver `post_recv` | Total NIC doorbells |
|--------|-------------------|-------------------|---------------------|---------------------|-------------------|
| **Credit Ring (current)** | 1M | 1M | 1M (credits) | 1M (doorbells) | 4M |
| **Credit Ring (batched N=8)** | 1M | 1M | 125K | 1M | 3.125M |
| **Read Ring + Sel. Signal** | 1M | 125K (N=8) | **0** | 1M | **2.125M** |

**Note**: Every received message requires an `ibv_post_recv` to repost the 4-byte
doorbell WR — this is a NIC doorbell that cannot be eliminated without switching
to a polling-based recv model (see §"No-Doorbell Polling Mode"). The Read Ring
advantage over batched Credit Ring is **32%** (2.125M vs 3.125M), primarily from
eliminating the receiver's credit Send+Imm WRs.

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
  repost doorbell   │ ████    │     │ ████    │      │ ████    │  ← same
  (ibv_post_recv)   │ ~120ns  │     │ ~120ns  │      │ ~120ns  │
                    ├─────────┤     ├─────────┤      ├─────────┤
  offset buf write  │         │     │         │      │ ▏       │  ← ~1ns
                    │         │     │         │      │         │
                    └─────────┘     └─────────┘      └─────────┘
  Total recv CPU:    ~620ns          ~271ns            ~221ns

  Savings vs current:  —             56%               64%
```

**Note**: `ibv_post_recv` for a 1-SGE 4-byte WR is estimated at ~120ns (WQE setup +
MMIO doorbell write). Published benchmarks on ConnectX-5/6 show 100–200ns. The exact
cost should be benchmarked on the target NIC; the relative ranking is stable across
the plausible range.

### Transport Tier Summary

| Tier | Transport | Recv CPU/msg | Use case |
|------|----------|-------------|----------|
| **Simple** | Credit Ring (unbatched) | ~620ns | Development, correctness testing |
| **Balanced** | Credit Ring (batched N=8) | ~271ns | Production gRPC, congested rings, fan-in |
| **Lowest-CPU** | Read Ring + Sel. Signal + Inline | ~221ns | HFT, low-utilization rings |

### Ring Sizing for High Message Rates

The default 64KB ring with 1200B messages holds ~54 messages = **10.8µs buffer at
5M msg/sec**. Any receiver scheduling jitter >11µs triggers backpressure Reads.

| Target rate | Required buffer | Ring capacity | Mode |
|-------------|----------------|---------------|------|
| 1M msg/sec | ~54µs (OK for shared core) | 64 KB (default) | Small ring |
| 5M msg/sec | ~100µs jitter absorption | ~600 KB | Large ring |
| 10M msg/sec | ~100µs jitter absorption | ~1.2 MB | Large ring |

For >5M msg/sec, use `ReadRingConfig { ring_capacity: 1 << 20, .. }` (1MB) with
large ring mode auto-enabled. Dedicated-core deployments with busy-poll (`tokio`
current-thread) can tolerate smaller rings.

## Implementation Plan

### New Components

| Component | Effort |
|-----------|--------|
| `ReadRingTransport` struct | Small — mirrors Credit Ring, minus credit fields, plus offset buffer |
| Offset buffer MR (per side) | Small — 4-byte MR, register in single contiguous allocation |
| **Second Memory Window** (offset buffer) | Small — `bind_offset_mw` with `IBV_ACCESS_REMOTE_READ` (NOT reuse `bind_recv_mw`) |
| Token expansion (20 → 32 bytes) | Small — version byte + `offset_va` + `offset_rkey` fields |
| `ReadRingConfig` + `TransportBuilder` impl | Small — mirrors `CreditRingConfig`, adds `min_free_threshold` |
| RDMA Read WR posting | Medium — new `WrOpcode::RdmaRead` path, async completion wait |
| `repost_recv` (no credit WR) | Small — chase-forward release + **write offset buffer** (AtomicU32) + repost doorbell |
| **CompletionTracker + slot_lengths** | Small — tracker + `Box<[usize]>` per-slot byte lengths for variable-size messages |
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

## Files

| File | Description |
|------|-------------|
| `rdma-io/src/read_ring_transport.rs` | ReadRingTransport implementation (~1200 lines) |
| `rdma-io/src/ring_common.rs` | Shared ring infrastructure (~360 lines) |
| `rdma-io/src/credit_ring_transport.rs` | Refactored to use `ring_common` (~1000 lines) |
| `rdma-io/src/transport.rs` | `Transport` + `TransportBuilder` traits (unchanged) |
| `rdma-io-tests/tests/read_ring_transport_tests.rs` | 8 Read Ring integration tests |
| `rdma-io-tests/tests/async_stream_tests.rs` | 11 Read Ring stream tests (33 total) |

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
| Recv release data structure | Hash table (unbounded) | `Box<[bool]>` bitmap (bounded by `max_outstanding`) | Bounded slot count makes bitmap simpler and faster |
| Backpressure handling | `SendQueue` queues pending sends | Returns `Ok(0)` — caller retries | Matches our `Transport` trait contract; avoids internal queue complexity |
| MR allocation | Single contiguous MR for all buffers | Single contiguous MR | Single MR simplifies registration |
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

## Open Design Questions

### 1. RDMA Read in Synchronous `send_copy`

`send_copy(&mut self, data) -> Result<usize>` is synchronous. On backpressure, the sender
must post an RDMA Read and wait for the CQ completion — but that's an async operation.

**Options:**

| Approach | Pros | Cons |
|----------|------|------|
| **Busy-poll send CQ inline** | Simple, no state machine | Burns CPU during stall |
| **Return `Ok(0)`, pick up Read result later** | Non-blocking, matches trait | Needs `read_in_flight` state flag + wr_id tag |
| **Post Read speculatively on every send** | Always up-to-date cache | Wasteful when ring isn't full |

**Recommended:** Return `Ok(0)` and post a non-blocking Read. On next `send_copy`,
check `read_in_flight` — if the Read has completed (poll send CQ), update
`cached_remote_head` and retry. This matches the existing `drain_recv_credits`
pattern in CreditRingTransport.

```rust
fn send_copy(&mut self, data: &[u8]) -> Result<usize> {
    // Try inline send CQ poll to pick up completed Read.
    if self.read_in_flight {
        self.drain_send_cq_for_read();
    }

    // Wrap-aware free space calculation (NOT wrapping_sub — that gives
    // wrong results when head < tail with positions in [0, capacity)).
    let free = if self.cached_remote_head >= self.write_tail {
        // No wrap: head is ahead of or at tail (could also mean empty ring)
        self.cached_remote_head - self.write_tail
    } else {
        // Wrapped: free space spans end + beginning
        self.remote_capacity - self.write_tail + self.cached_remote_head
    };

    if free < data.len() + self.min_free_threshold {
        if !self.read_in_flight {
            self.post_offset_read()?;  // non-blocking
            self.read_in_flight = true;
        }
        return Ok(0);  // caller retries via poll_writable
    }

    // Proactive Read when space is getting low.
    if free < data.len() * 2 + self.min_free_threshold && !self.read_in_flight {
        self.post_offset_read()?;
        self.read_in_flight = true;
    }

    // ... proceed with Write+Imm
}
```

### 2. RDMA Read CQ Routing

RDMA Read is an "initiator" operation — its completion arrives on the **send CQ**
(not recv CQ). The send CQ will contain mixed completion types:

| WR type | wr_id | Opcode in WC |
|---------|-------|-------------|
| Write+Imm (data) | `release_bytes` | `WcOpcode::RdmaWrite` |
| Write+Imm (padding) | `PADDING_SENTINEL` | `WcOpcode::RdmaWrite` |
| **RDMA Read** | **`READ_SENTINEL`** | **`WcOpcode::RdmaRead`** |

The `poll_send_completion` / `drain_send_cq` paths must handle `WcOpcode::RdmaRead`:
- Update `cached_remote_head` from the Read result (4 bytes in local read buffer)
- Set `read_in_flight = false`
- Do NOT release send ring space (Read doesn't consume ring bytes)
- Do NOT count as "≥1 send completed" for the `poll_send_completion` return —
  a Read completion is a cache refresh, not a data send. If `poll_send_completion`
  returns `Ready` solely due to a Read, `AsyncRdmaStream::poll_write` would retry
  `send_copy` on a still-full ring, causing a busy-spin. Read completions should
  be processed silently; only Write+Imm completions trigger the `Ready` return.

**RDMA Read does NOT increment `send_in_flight`** — it's not a data send and doesn't
occupy send ring space. The `read_in_flight: bool` flag tracks it separately.

### 3. Offset Buffer Atomicity

The receiver writes a `u32` to the offset buffer; the sender reads it via RDMA Read
(NIC-level DMA, not CPU load). Correctness requires:

- **Atomic store**: Use `AtomicU32::store(head, Ordering::Release)` on the receiver
  side. This ensures correctness on all architectures (x86, ARM64, POWER) without
  relying on platform-specific memory ordering guarantees.
- **RDMA NIC**: PCIe TLP minimum payload is 4 bytes — a 4-byte aligned RDMA Read
  returns a consistent value from the coherence point.
- **Cache coherence**: On x86, PCIe devices participate in the cache coherence domain.
  On ARM64 (Graviton, BlueField), `ibv_reg_mr` typically provides DMA-coherent
  mappings. The `Ordering::Release` fence ensures the store is visible at the
  coherence point before any subsequent operation.
- **Alignment**: Offset buffer must be 4-byte aligned (guaranteed by allocation
  layout; recommend 64-byte alignment for cache-line isolation — see §4).

### 4. Offset Buffer Cache Line Alignment

The 4-byte offset buffer should be **64-byte aligned** to avoid false sharing:

```
  BAD:  [... recv_ring last 60 bytes ...][OBuf 4B]  ← same cache line
  GOOD: [... recv_ring ...][ 60B pad ][OBuf 4B]     ← own cache line
```

The receiver writes `offset_buffer` on every `repost_recv`. If it shares a cache
line with the recv ring tail, every data Write+Imm from the sender invalidates the
cache line containing the offset buffer, causing unnecessary cache thrashing.

**Fix:** Pad the allocation so the offset buffer starts at a 64-byte boundary.
The overhead is at most 60 bytes per connection — negligible.

### 5. Disconnect Detection via Failed Read

If the remote QP enters ERROR state (peer disconnected), an RDMA Read will fail
with `IBV_WC_REM_ACCESS_ERR` or `IBV_WC_WR_FLUSH_ERR`. The Read completion handler
must set `qp_dead = true` on failure, same as the existing Write+Imm error path.

### 6. MW Invalidation on Disconnect

On disconnect, post `IBV_WR_LOCAL_INV` for both MWs before QP teardown to
explicitly revoke the peer's access. Order: MW2 (offset buffer — stops Reads)
first, then MW1 (recv ring — stops Writes). Wait for send CQ completions of
both invalidation WRs. This is a security hardening that CreditRingTransport
also needs (documented as future work in `rdma-credit-ring-transport.md`).

## Possible Improvements

### Adaptive Flow Control

A hybrid `AdaptiveRingTransport` could switch between Credit and Read behavior
based on observed ring utilization:

```
  utilization > 70% sustained → credit mode (proactive pushes, no stalls)
  utilization < 30% sustained → read mode (zero receiver WRs)
  30-70% → read mode with proactive Reads
```

This gets the best of both worlds without requiring the user to choose. The
implementation would track a moving average of `free_space / capacity` and flip
a `flow_mode: enum { Credit, Read }` flag. The receiver would either post
Send+Imm credits or just write to the offset buffer depending on the mode.

**Complexity:** Medium — both code paths needed, plus hysteresis to avoid flapping.
**Priority:** P3 (after both transports are validated independently).

### Read Coalescing

Instead of reading just 4 bytes (offset buffer), read a 64-byte region that
includes diagnostic counters alongside the head position:

```
  Offset region (64 bytes, cache-line aligned):
  ┌───────┬──────────┬──────────┬──────────────────────────────┐
  │ head  │ recv_cnt │ drop_cnt │ reserved                     │
  │ (4B)  │ (4B)     │ (4B)     │ (52B)                        │
  └───────┴──────────┴──────────┴──────────────────────────────┘
```

An RDMA Read of 4 bytes vs 64 bytes costs the same on the NIC (minimum PCIe TLP
is 64 bytes). The extra counters provide free telemetry for monitoring/debugging.

### No-Doorbell Polling Mode

For ultra-low-latency scenarios, eliminate the doorbell recv entirely. The sender
writes data AND increments a "sequence counter" in a separate sender-to-receiver
region. The receiver **polls** this counter instead of waiting for CQ notifications:

```
  Sender: Write+Imm(data) + atomic Write(seq_counter++)
  Receiver: spin-poll seq_counter → new data → read from ring
```

This saves 1 `ibv_post_recv` + 1 CQ notification per message on the receiver.
Trade-off: burns a CPU core on the receiver (DPDK-style). Only for dedicated
HFT-class deployments.

**Priority:** P4 — only if benchmarks show doorbell CQ overhead is the bottleneck.

## Priority

**P2** — implemented. Large ring mode (Phase 3) is deferred. See
[RingPerformance.md](../future/RingPerformance.md) for the full priority roadmap.
