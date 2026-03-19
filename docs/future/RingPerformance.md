# Ring Transport Family — Performance & Architecture

**Status:** Future work  
**Date:** 2026-03-19  
**Implementation:** `rdma-io/src/credit_ring_transport.rs` (current: credit-based)  
**Prerequisite:** [rdma-ring-transport.md](../design/rdma-ring-transport.md) — current design

## Transport Family Overview

All Write+Ring transports share the same data path (`memcpy` → `ibv_post_send(WRITE_WITH_IMM)`)
but differ in **flow control**. Rather than optimizing one transport for all workloads, we
provide two sibling implementations behind the same `Transport` trait:

| Transport | Flow Control | Rename |
|-----------|-------------|--------|
| `CreditRingTransport` | Receiver pushes credits (SendWithImm) | Current `CreditRingTransport` |
| `ReadRingTransport` | Sender polls remote head (RDMA Read) | New — msquic approach |

Both share ~80% of the code: ring buffers, token exchange, doorbell recv, imm encoding,
MW binding, virtual buffer index mapping. They differ only in `repost_recv` (receiver side)
and the sender's space-check path.

```rust
// User selects transport by config — same AsyncRdmaStream<T> interface
let stream = AsyncRdmaStream::new(CreditRingConfig::default().connect(&addr).await?);
let stream = AsyncRdmaStream::new(ReadRingConfig::default().connect(&addr).await?);
```

See [rdma-transport-layer.md](../design/rdma-transport-layer.md#transport-naming-convention-planned)
for the full naming convention (including `SendRecvTransport` rename).

### When to Use Each

| Scenario | Best Transport | Why |
|----------|---------------|-----|
| Congested ring (>70% utilization) | **Credit Ring** | Proactive push — sender learns of space immediately |
| Backpressure-heavy workloads | **Credit Ring** | Zero-latency recovery — credit arrives, no polling needed |
| Fan-in (many senders → one receiver) | **Credit Ring** | O(1) push per sender vs. O(N) RDMA Reads on receiver NIC |
| Low-moderate load, CPU-sensitive | **Read Ring** | Zero receiver WRs — offset buffer write is ~1ns |
| HFT / >5M msg/sec | **Read Ring** | Minimal NIC doorbells — 1M doorbells for 1M messages |
| Bulk transfer (large ring, few stalls) | **Read Ring** | Reads are rare when ring rarely fills |

## Hot-Path Comparison

### Per-Message Send (Hot Path)

| Step | Credit Ring | Read Ring (msquic) | rsocket |
|------|------------|-------------------|---------|
| Space check | 1 compare (`remote_credits`) | 1 compare (`cached_remote_head` vs tail) | 1 compare (`sseq_no < limit`) |
| Copy to ring | `memcpy` → send_ring | `memcpy` → send ring | `memcpy` → sbuf |
| Post WR | 1 `ibv_post_send(WRITE_WITH_IMM)` | 1 `ibv_post_send(WRITE_WITH_IMM)` | 1–2 WRs (iWARP needs 2) |
| **Total copies** | **1** | **1** | **1** |
| **Syscalls** | **1** (post_send) | **1** | **1–2** |

### Per-Message Recv (Overhead Comparison)

| Step | Credit Ring | Read Ring (msquic) | rsocket |
|------|------------|-------------------|---------|
| CQ poll | `ibv_poll_cq` | `ibv_poll_cq` | `rpoll` |
| Decode imm | Shift + mask | Shift + mask | Shift + mask |
| Repost doorbell | 1 `ibv_post_recv` (4 bytes) | 1 `ibv_post_recv` (4 bytes) | — |
| **Flow control** | **1 `ibv_post_send(SendWithImm)`** | **1 `u32` store (~1ns)** | **1 `ibv_post_send(Write)` per batch** |
| Completion tracking | Direct head advance | Direct head advance | Sequence compare |
| Lock contention | None (`&mut self`) | None (`&mut self`) | Global lock |

**Key difference:** Credit Ring receiver posts a Send+Imm WR per `repost_recv()`.
Read Ring receiver writes a `u32` to a local offset buffer — the sender reads it
on demand via RDMA Read.

## `CreditRingTransport` Improvements

The following improvements apply to `CreditRingTransport` specifically.

### Credit Batching

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
pub struct CreditRingConfig {
    pub ring_capacity: usize,
    pub max_message_size: usize,
    pub token_timeout: Duration,
    pub max_inline_data: u32,
    pub credit_batch_size: usize,  // NEW: default 4
}
```

## `ReadRingTransport` — RDMA Read Flow Control

**Status:** Separate transport (sibling to Credit Ring)  
**Complexity:** Medium — ~300 new lines + shared ring infrastructure  
**Priority:** P2 (implement after Credit Ring is production-validated)

The Read Ring transport eliminates all credit WRs by using RDMA Read for flow
control, following the msquic approach.

### Design

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

### Credit Ring vs. Read Ring

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
| Complexity | Low | Medium |

**Key insight:** Credit Ring wins under congestion (proactive push, no polling).
Read Ring wins under low load (zero receiver WRs when Reads are rare).

### Crossover Model

```
Credit batch WRs = send_rate / N   (e.g., 125K/s at 1M msg/s, N=8)
RDMA Read WRs   ≈ send_rate × f(ring_utilization)

Crossover: at ~70% ring utilization with small rings, Read frequency
exceeds credit batch frequency. Above this, Credit Ring has fewer total
WRs AND avoids RTT stalls on the sender hot path.
```

### Implementation

New files/changes needed beyond shared ring infrastructure:

| Component | Effort |
|-----------|--------|
| `ReadRingTransport` struct | Small — mirrors Credit Ring, minus credit fields |
| Offset buffer MR (per side) | Small — 4-byte MR, register + share in token |
| Token expansion (20 → 28 bytes) | Small — add `offset_va` + `offset_rkey` fields |
| `ReadRingConfig` + `TransportBuilder` impl | Small — mirrors `CreditRingConfig` |
| RDMA Read WR posting | Medium — new `WrOpcode::RdmaRead` path, async completion wait |
| `repost_recv` (no credit WR) | Small — advance head + write offset buffer + repost doorbell |
| `send_copy` space check (Read path) | Medium — cache remote head, RDMA Read on backpressure |

**Shared code** (extracted from current `CreditRingTransport`): ring buffers, imm encoding,
token exchange, MW binding, virtual buffer mapping, doorbell recv, send path (Write+Imm).

**Total: ~300 new lines for Read Ring + ~200 lines refactored into shared module.**

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

## Shared Improvements (Both Transports)

The following improvements apply to both Credit Ring and Read Ring transports.

### Inline Small Messages

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

**Config:** Already exists — `CreditRingConfig::max_inline_data` (default 0). Just
needs to be wired into the send path.

### Selective Signaling

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

**Prerequisite:** Selective signaling is orthogonal to flow control. Can be
applied to both Credit Ring and Read Ring transports.

### SRQ (Shared Receive Queue) for Doorbells

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
   Credit Batch ●   │           ● Read Ring Transport
   (P0, Low)        │           (P2, Medium)
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
1. **Credit batching** — biggest bang for the buck, simple change to Credit Ring
2. **Inline small messages** — wire existing config field into send path (both transports)
3. **Selective signaling** — reduces CQ overhead (both transports)
4. **Read Ring transport** — extract shared ring code, implement RDMA Read flow control
5. **SRQ doorbells** — only for many-peer scenarios (both transports)

## Read Ring: Lowest-CPU Design

The Read Ring transport combined with Selective Signaling + Inline yields a design
where the NIC does almost all the work and CPU touches are minimized:

### Per-Message CPU Cost Comparison

| Work item | Credit Ring (current) | Credit Ring (batched N=8) | **Read Ring + Sel. Signal** |
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

### Read Ring Architecture

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

### When to Use Each Tier

| Tier | Transport | Recv CPU/msg | Use case |
|------|----------|-------------|----------|
| **Simple** | Credit Ring (unbatched) | ~550ns | Development, correctness testing |
| **Balanced** | Credit Ring (batched N=8) | ~200ns | Production gRPC, congested rings, fan-in |
| **Lowest-CPU** | Read Ring + Sel. Signal + Inline | ~151ns | HFT, >5M msg/sec, low-utilization rings |

### Implementation Phases

```
  Phase 1 (P0): Credit Batching [Credit Ring]
    └── Simple, ~200 lines, 63% recv CPU reduction
  
  Phase 2 (P1): Inline Small Messages [Both transports]
    └── Wire existing config field, ~20 lines, latency reduction for small msgs
  
  Phase 3 (P2): Selective Signaling [Both transports]
    └── Reduce sender CQ overhead by N×, ~100 lines
  
  Phase 4 (P2): Read Ring Transport [New transport]
    └── Extract shared ring code (~500 lines) + Read Ring impl (~300 lines)
    └── Requires: offset buffer MR, RDMA Read WR path, ReadRingConfig
    └── Total recv overhead: ~151ns (73% reduction vs unbatched Credit Ring)
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
