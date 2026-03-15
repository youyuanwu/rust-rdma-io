# Ring Transport Performance Improvements

**Status:** Future work  
**Date:** 2026-03-15  
**Implementation:** `rdma-io/src/rdma_ring_transport.rs`  
**Prerequisite:** [rdma-ring-transport.md](../design/rdma-ring-transport.md) — current design

## Hot-Path Comparison

All three Write+Ring transports (ours, msquic, rsocket) execute the same
fundamental operation per message: `memcpy` → `ibv_post_send(WRITE_WITH_IMM)`.
The differences are in flow control overhead and completion tracking.

### Per-Message Send (Hot Path)

| Step | Our Ring | msquic | rsocket |
|------|---------|--------|---------|
| Credit/space check | 1 compare (`remote_credits`) | 1 compare (ring free space) | 1 compare (`sseq_no < limit`) |
| Copy to ring | `memcpy` → send_ring | `memcpy` → send ring | `memcpy` → sbuf |
| Post WR | 1 `ibv_post_send(WRITE_WITH_IMM)` | 1 `ibv_post_send(WRITE_WITH_IMM)` | 1–2 WRs (iWARP needs 2) |
| **Total copies** | **1** | **1** | **1** |
| **Syscalls** | **1** (post_send) | **1** | **1–2** |

### Per-Message Recv (Overhead Comparison)

| Step | Our Ring | msquic | rsocket |
|------|---------|--------|---------|
| CQ poll | `ibv_poll_cq` | IOCP callback | `rpoll` |
| Decode imm | Shift + mask | Shift + mask | Shift + mask |
| Repost doorbell | 1 `ibv_post_recv` (4 bytes) | 1 `ibv_post_recv` (4 bytes) | — |
| **Credit update** | **1 `ibv_post_send(SendWithImm)`** | **None (local write)** | **1 `ibv_post_send(Write)` per batch** |
| Completion tracking | Direct head advance | Hash table lookup | Sequence compare |
| Lock contention | None (`&mut self`) | Per-connection mutex | Global lock |

**Key gap:** Our receiver posts a Send+Imm WR for every `repost_recv()`. msquic
posts zero WRs — it just writes to a local offset buffer that the sender reads
on demand.

## Improvement 1: Credit Batching

**Impact:** High — reduces credit WR count by N×  
**Complexity:** Low  
**Priority:** P0

Instead of sending a credit update on every `repost_recv()`, accumulate freed
credits and send every N reposts:

```rust
// Current: 1 credit WR per repost (100% overhead for small messages)
fn repost_recv(&mut self, buf_idx: usize) {
    self.local_freed_credits += 1;
    let imm = self.local_freed_credits as u32;
    post_send(SendWithImm(imm));  // ← every time
}

// Proposed: 1 credit WR per N reposts
fn repost_recv(&mut self, buf_idx: usize) {
    self.local_freed_credits += 1;
    self.unsent_credits += 1;
    if self.unsent_credits >= self.credit_batch_size {
        let imm = self.local_freed_credits as u32;
        post_send(SendWithImm(imm));
        self.unsent_credits = 0;
    }
}
```

**Absolute encoding makes this safe.** The credit value is the total freed count,
not a delta. If credits 1–7 are batched into one update with value `7`, the sender
computes `delta = 7 - 0 = 7` and recovers all 7 credits at once. No credits lost.

### Trade-offs

| Batch size (N) | Credit WRs at 1M msg/s | Sender stall risk | Notes |
|----------------|----------------------|-------------------|-------|
| 1 (current) | 1M/s (100% overhead) | None | Simple, correct |
| 4 | 250K/s (25% overhead) | Low — 4 msg delay | Good default |
| 8 | 125K/s (12.5% overhead) | Moderate — 8 msg delay | Best throughput |
| 16 | 62.5K/s (6.25% overhead) | Higher — sender may stall | Only for bulk transfer |

**Stall risk:** With batch size N, the sender can exhaust credits while the
receiver has freed N-1 credits but hasn't hit the batch threshold. Mitigation:
always flush unsent credits when `local_freed_credits == max_outstanding` (all
buffers returned).

### Config

```rust
pub struct RingConfig {
    pub ring_capacity: usize,
    pub max_message_size: usize,
    pub token_timeout: Duration,
    pub max_inline_data: u32,
    pub credit_batch_size: usize,  // NEW: default 4
}
```

## Improvement 2: RDMA Read Direct Space Check (msquic approach)

**Impact:** Eliminates all credit WRs  
**Complexity:** High  
**Priority:** P2 (only if credit batching is insufficient)

msquic avoids credit messages entirely. Instead:

1. Each side allocates a 4-byte "offset buffer" MR containing the current ring head
2. Token exchange includes the offset buffer's VA + rkey (token grows 20 → 28 bytes)
3. Sender tracks `cached_remote_head` locally
4. Before sending: `free_space = cached_remote_head - write_tail` (with wrap)
5. If insufficient: post `RDMA Read` of remote offset buffer → update cache
6. Receiver: after consuming data, writes new head to local offset buffer (no WR)

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

### Advantages Over Credit Batching

| Aspect | Credit Batching | RDMA Read Check |
|--------|----------------|----------------|
| Receiver WRs per msg | 1/N | **0** |
| Sender WRs per msg | 1 | 1 (+ occasional Read) |
| Total WRs at 1M msg/s (N=8) | 1.125M | ~1.001M |
| Backpressure latency | Wait for credit Send RTT | Wait for Read RTT |
| Self-healing on loss | ✅ (absolute encoding) | ✅ (Read always gets latest) |
| Complexity | Low | High |

### Implementation Cost

| Component | Effort |
|-----------|--------|
| Offset buffer MR (per side) | Small — 4-byte MR, register + share in token |
| Token expansion (20 → 28 bytes) | Small — add 2 fields (offset_va, offset_rkey) |
| RDMA Read WR posting | Medium — new `WrOpcode::RdmaRead` path, async completion wait |
| Read/Write race handling | Medium — sender may read stale head; must handle gracefully |
| Remove credit Send+Imm path | Medium — remove from `repost_recv`, `drain_recv_credits`, `poll_recv` |
| MW for offset buffer | Small — bind separate MW for the 4-byte offset MR |

**Total: ~200-300 lines changed, moderate risk (race conditions).**

### Race Condition

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

## Improvement 3: Inline Small Messages

**Impact:** Medium — saves PCIe DMA for small payloads  
**Complexity:** Low  
**Priority:** P1

RDMA inline data avoids a PCIe DMA read for the send buffer. The NIC copies
data directly from the WQE in host memory.

```rust
// Current: always use SGE (PCIe DMA read for any size)
let sge = Sge::new(send_ring.addr() + offset, data_len, send_ring.lkey());
let wr = SendWr::new(...).sg(sge);

// Proposed: use INLINE for small messages
let wr = if data_len <= config.max_inline_data as usize {
    SendWr::new(...).flags(SendFlags::SIGNALED | SendFlags::INLINE).sg(sge)
} else {
    SendWr::new(...).flags(SendFlags::SIGNALED).sg(sge)
};
```

**Threshold:** Typical `max_inline_data` is 64–256 bytes depending on HCA.
QUIC short headers are ~20-40 bytes. ACKs and small control packets benefit most.

**Config:** Already exists — `RingConfig::max_inline_data` (default 0). Just
needs to be wired into the send path.

## Improvement 4: Selective Signaling

**Impact:** Medium — reduces send CQ polling overhead  
**Complexity:** Medium  
**Priority:** P2

Currently `sq_sig_all: true` — every send WR generates a CQ completion.
With selective signaling, only every Nth send is signaled:

```
Send WRs: [unsig] [unsig] [unsig] [SIGNALED] [unsig] [unsig] [unsig] [SIGNALED]
CQ polls:                           ^                                   ^
```

**Benefit:** Fewer CQ completions to process, fewer `ibv_poll_cq` syscalls.
**Risk:** Must track which WRs are in-flight for `send_ring.release()`.
Currently wr_id encodes bytes-to-release — with selective signaling, the
signaled WR must release ALL preceding unsignaled WRs' space.

**Prerequisite:** Selective signaling is orthogonal to credit mechanism. Can be
combined with either credit batching or RDMA Read approach.

## Improvement 5: SRQ (Shared Receive Queue) for Doorbells

**Impact:** High for many-peer scenarios  
**Complexity:** Medium  
**Priority:** P2

Currently each connection pre-posts `max_outstanding` doorbell recv WRs (4 bytes
each). With 20 peers × 43 outstanding = 860 recv WRs.

SRQ pools recv WRs across connections:

```
Current:  Peer1: [d0][d1]...[d42]   Peer2: [d0][d1]...[d42]   = 86 recv WRs
SRQ:      Shared: [d0][d1]...[d63]                              = 64 recv WRs
```

**Benefit:** Memory scales with total concurrent messages, not peers × outstanding.

## Priority Roadmap

```
                  Impact
                    ▲
                    │
   Credit Batch ●   │           ● RDMA Read Check
   (P0, Low)        │           (P2, High)
                    │
   Inline Small ●   │   ● Selective Signaling
   (P1, Low)        │   (P2, Medium)
                    │
                    │       ● SRQ Doorbells
                    │       (P2, Medium)
                    │
                    └──────────────────────► Complexity
```

**Recommended order:**
1. **Credit batching** — biggest bang for the buck, simple change
2. **Inline small messages** — wire existing config field into send path
3. **Selective signaling** — reduces CQ overhead, moderate complexity
4. **RDMA Read check** — only if profiling shows credits are still a bottleneck
5. **SRQ doorbells** — only for many-peer scenarios

## Lowest-CPU Design: Combined Optimizations

Combining RDMA Read + Selective Signaling + Inline yields a design where the
NIC does almost all the work and CPU touches are minimized:

### Per-Message CPU Cost Comparison

| Work item | Current | Credit batch (N=8) | **Lowest-CPU** |
|-----------|---------|-------------------|---------------|
| **Sender: build WR** | 1 WR | 1 WR | 1 WR |
| **Sender: post send** | 1 `ibv_post_send` | 1 `ibv_post_send` | 1 `ibv_post_send` (INLINE for small) |
| **Sender: poll send CQ** | 1 entry | 1 entry | **1/N entries** (selective signaling) |
| **Sender: process credit CQ** | 1 entry | 1/8 entry | **0** (no credit WRs) |
| **Sender: RDMA Read** | 0 | 0 | Rare (only on backpressure) |
| **Receiver: post credit WR** | 1 `ibv_post_send` (~300ns) | 1/8 `ibv_post_send` | **0** |
| **Receiver: build credit WR** | Fill SendWr fields | 1/8 of that | **0** |
| **Receiver: update offset buf** | N/A | N/A | **1 `u32` store (~1ns)** |
| **Receiver: poll credit send CQ** | 1 entry | 1/8 entry | **0** |

### Architecture

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

### Cost Per 1M Messages/sec

| Design | Sender `ibv_post_send` | Sender CQ entries | Receiver `ibv_post_send` | Receiver CQ entries | Total NIC doorbells |
|--------|----------------------|-------------------|------------------------|--------------------|-------------------|
| **Current** | 1M | 1M | 1M (credits) | 1M (credits) | 3M |
| **Credit batch (N=8)** | 1M | 1M | 125K | 125K | 2.125M |
| **Lowest-CPU** | 1M | 125K (N=8) | **0** | **0** | **1M** |

The lowest-CPU design achieves **1M NIC doorbells for 1M messages** — the
theoretical minimum (one post_send per message, everything else is either
local memory or NIC-served).

### Receiver CPU Breakdown

```
                    Current         Credit Batch      Lowest-CPU
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

### When to Use Each Tier

| Tier | Design | Recv CPU/msg | Use case |
|------|--------|-------------|----------|
| **Simple** | Current (credit per repost) | ~550ns | Development, correctness testing |
| **Balanced** | Credit batch (N=8) | ~200ns | Production gRPC, moderate throughput |
| **Lowest-CPU** | RDMA Read + Sel. Signal + Inline | ~151ns | HFT, >5M msg/sec, CPU-bound workloads |

### Implementation Phases

```
  Phase 1 (P0): Credit Batching
    └── Simple, ~200 lines, 63% recv CPU reduction
  
  Phase 2 (P1): Inline Small Messages
    └── Wire existing config field, ~20 lines, latency reduction for small msgs
  
  Phase 3 (P2): Selective Signaling
    └── Reduce sender CQ overhead by N×, ~100 lines
  
  Phase 4 (P2): RDMA Read Check
    └── Eliminate all credit WRs, ~300 lines, 73% recv CPU reduction total
    └── Requires: offset buffer MR, RDMA Read WR path, token expansion
```

## Benchmarking Plan

Before implementing optimizations, establish baselines:

| Metric | Tool | Target |
|--------|------|--------|
| Messages/sec (1.2KB QUIC) | Custom bench: `send_copy` + `poll_recv` loop | Baseline |
| Latency p50/p99 (echo) | Custom bench: timestamps around send/recv | < 10µs p50 |
| Credit WR overhead | Count WRs in `poll_send_completion` | Measure % of total |
| CQ poll frequency | Instrument `poll_completions` call count | Baseline |
| Quinn throughput | `iperf3` over Quinn + ring transport | vs Send/Recv transport |

After each optimization, re-run benchmarks to validate improvement and catch regressions.
