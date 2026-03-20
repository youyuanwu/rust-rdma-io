# Ring Transport Family — Performance Roadmap

**Status:** Future work  
**Date:** 2026-03-20  
**Implementation:** `rdma-io/src/credit_ring_transport.rs` (current: credit-based)  
**Prerequisite:** [rdma-credit-ring-transport.md](../design/rdma-credit-ring-transport.md) — CreditRingTransport design  
**See also:** [rdma-read-ring-transport.md](../design/rdma-read-ring-transport.md) — ReadRingTransport design

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

See [rdma-read-ring-transport.md](../design/rdma-read-ring-transport.md) for the full design,
including architecture, sequence diagrams, race condition analysis, performance comparisons,
and implementation plan.

**Summary:** Eliminates all credit WRs by using RDMA Read for flow control (msquic approach).
Receiver writes a `u32` to a local offset buffer (~1ns); sender reads it on demand.
Best for HFT / >5M msg/sec, low-utilization rings, CPU-sensitive workloads.

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
where the NIC does almost all the work and CPU touches are minimized.

See [rdma-read-ring-transport.md](../design/rdma-read-ring-transport.md#performance-comparison)
for the full performance analysis, including per-message CPU cost comparison, receiver CPU
breakdown diagrams, cost-per-1M-messages tables, and transport tier summary.

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
