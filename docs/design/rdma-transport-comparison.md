# RDMA Transport Design Comparison

**Status:** Implemented (both transports)  
**Updated:** 2026-03-15  
**Scope:** Four RDMA transport designs compared: our Send/Recv, our Write+Ring, msquic's Write+Ring, rsocket's Write+Ring

## Overview

Four production/proposed RDMA transports use fundamentally different approaches to deliver
data over RDMA. We implement **two** — selectable via `TransportBuilder` at construction time:

| | **rust-rdma-io Send/Recv** | **rust-rdma-io Ring** | **msquic RDMA (PR #5113)** | **rsocket (librdmacm)** |
|---|---|---|---|---|
| **Type** | `SendRecvTransport` | `CreditRingTransport` | N/A (C) | N/A (C) |
| **Builder** | `SendRecvConfig` | `CreditRingConfig` | — | — |
| **Purpose** | Byte stream (gRPC) + Datagram (QUIC) | Datagram (QUIC) + Bulk transfer | QUIC packet delivery | POSIX socket drop-in |
| **Platform** | Linux (ibverbs / rdma_cm) | Linux (ibverbs / rdma_cm) | Windows (NDSPI / MANA) | Linux (ibverbs / rdma_cm) |
| **Code size** | ~500 lines | ~1300 lines | ~6000 lines | ~3400 lines |
| **Language** | Rust (async) | Rust (async) | C (IOCP callbacks) | C (blocking + rpoll) |
| **iWARP** | ✅ All providers | ❌ InfiniBand/RoCE only | ❌ MANA only | ⚠️ Adapts (2 WRs) |

```rust
// User selects transport at construction time — consumers are generic
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::credit_ring_transport::CreditRingConfig;

let incoming = RdmaIncoming::bind(&addr, SendRecvConfig::stream())?;  // Send/Recv
let incoming = RdmaIncoming::bind(&addr, CreditRingConfig::default())?;      // Write+Ring
```

## Data Transfer Verb

The most fundamental design choice: how data moves between peers.

### Send/Recv (Two-Sided) — `SendRecvTransport`

```
Sender:                          Receiver:
  memcpy data → send MR           Pre-post recv buffer (ibv_post_recv)
  ibv_post_send(SEND)             HCA matches incoming → recv buffer
  HCA DMAs from send MR ──────── HCA DMAs into recv buffer
  SendCQ completion               RecvCQ completion
                                  Application reads recv buffer
```

**How it works:** Both sides participate. Sender posts a Send WQE, receiver pre-posts a Recv WQE.
The HCA matches them — sender's data lands in the next available recv buffer.

### RDMA Write + Ring Buffer (One-Sided) — `CreditRingTransport`, msquic, rsocket

```
Sender:                          Receiver:
  memcpy data → send ring          (nothing — passive)
  ibv_post_send(WRITE_WITH_IMM)    HCA writes directly into recv ring
  HCA DMAs from send ring ──────── Data lands at remote ring[offset]
  SendCQ completion                RecvCQ fires (doorbell from Immediate Data)
                                   Application reads recv ring[offset]
```

**How it works:** Only the sender is active. Sender knows the remote ring buffer's virtual address
and rkey (exchanged during connection setup). The HCA writes directly into remote memory.
The receiver is notified via Immediate Data in the RecvCQ.

### Comparison

| Aspect | Send/Recv (ours) | RDMA Write + Ring (ours / msquic / rsocket) |
|--------|------------------|------------------------------------|
| **Copies** | 2 (sender → send MR, HCA → recv buf) | 1 (sender → send ring, HCA → remote ring in-place) |
| **Receiver CPU** | HCA must match send to posted recv | Zero — data lands directly in app memory |
| **Pre-posted buffers** | Required (N recv WQEs) | Doorbell WRs only (4 bytes each) |
| **Flow control** | Hardware RNR retry (automatic) | Application-level credit tracking |
| **Notification** | Implicit (recv completion = data) | Explicit doorbell (Immediate Data) |
| **Setup** | None beyond QP creation | Token exchange (VA + rkey + capacity) |
| **Memory exposure** | None | Remote peer can write to MW-scoped recv ring region only |

## Ring Buffer Protocol Comparison

All three ring implementations (ours, msquic, rsocket) differ in details.

### Our Ring Buffer (`CreditRingTransport`)

```
┌────────────────────────┬────────────────────────┐
│   send_ring (64KB)     │   recv_ring (64KB)     │
│   Separate MR each     │   Separate MR each     │
│   [head ──── tail]     │   [head ──── tail]     │
└────────────────────────┴────────────────────────┘
  + N×4-byte doorbell MRs for recv WRs
```

- **Head/Tail pointers** with wrap-around; padding markers for ring-end gaps
- **Virtual buffer index slab** (`Vec<Option<(offset, len)>>`) maps ring offsets to consumer-facing indices
- **Immediate encoding:** Small-ring: `imm = (offset << 16) | length` — 16 bits each
- **Credit protocol:** Absolute freed count via `SendWithImm` — self-healing on loss
- **Backpressure:** `send_copy` returns `Ok(0)` on credit exhaustion; inline `drain_recv_credits()` recovers
- **Token exchange:** 20-byte `RingToken` via inline Send/Recv after connect/accept
- **Memory Windows:** MW Type 2 bound to recv ring; token carries `mw.rkey()`; panics if device doesn't support MW
- **iWARP:** Rejected at `connect()`/`accept()` — `any_device_is_iwarp()` check

### msquic Ring Buffer

```
┌──────────────────────────────────────────────────┐
│ Send Ring Buffer │ Recv Ring Buffer │ Ofs │ ROfs │
│ (64KB default)   │ (64KB default)   │ (4B)│ (4B) │
└──────────────────────────────────────────────────┘
  Single MR registration for entire region
```

- **Head/Tail pointers** with wrap-around
- **Completion hash table** for out-of-order release tracking
- **Two immediate data modes:**
  - Small ring (≤64KB): `imm = (offset << 16) | length` — 16-bit each
  - Large ring (>64KB): `imm = length` only; offset via RDMA Read of remote offset buffer
- **Backpressure:** Check remote ring space before send; queue to `SendQueue` if full
- **Token exchange:** Post-connect Send/Recv of `RDMA_DATAPATH_PRIVATE_DATA` (24 bytes: VA + capacity + rkey + offset VA + offset rkey)
- **Memory Windows:** Scope remote write access per-connection; invalidated on disconnect

### rsocket Ring Buffer

```
┌──────────────────────────┬──────────────────────────┐
│   sbuf (128KB default)   │   rbuf (128KB default)   │
│   [sgl_half_0][sgl_half_1] │   [sgl_half_0][sgl_half_1] │
└──────────────────────────┴──────────────────────────┘
  2 SGL entries cycling between buffer halves
```

- **Split-buffer design:** Ring divided into 2 halves (RS_SGL_SIZE=2), cycling between them
- **Sequence numbers:** `sseq_no` / `sseq_comp` for send tracking; `rseq_no` / `rseq_comp` for recv
- **Credit-based flow control:** Receiver sends `RS_OP_SGL` credits via RDMA Write to sender's `remote_sgl`
- **Immediate data format:** 3-bit opcode (bits 31:29) + 29-bit data (max 512MB per message)
- **Token exchange:** Embedded in CM private data during connect (`rs_conn_data` struct)
- **No Memory Windows:** Uses MR-level protection only — entire registered region exposed
- **Overlapped send sizing:** Starts at 2KB, doubles per iteration up to 64KB (RS_MAX_TRANSFER)
- **iWARP adaptation:** Detects `IBV_TRANSPORT_IWARP`, uses Send + Write (2 WRs per message) instead of Write-With-Immediate

### Send/Recv (`SendRecvTransport`) — No Ring

```
Pre-posted recv buffers (independent):
  [buf0] [buf1] [buf2] ... [buf31]
  Each receives exactly one message. Reposted after consumption.
```

- **No ring protocol:** No head/tail, no wrap-around, no completion tracking
- **No token exchange:** Two-sided verbs need no remote memory info
- **No memory exposure:** Receiver's buffers are never directly accessed by sender
- **Independent buffers:** Each recv buffer is a standalone slot

## Head-of-Line Blocking

### Wire Level (RC QP — All Designs)

All use RC (Reliable Connection) QPs, which provide TCP-like in-order delivery:

```
Sender posts: A → B → C
Packet B lost on wire:
  HCA holds C, retransmits B (invisible to software)
  Application sees: A ... [wait] ... B, C
```

This is inherent to RC and affects all equally. Practical impact is negligible in
datacenter networks (<10⁻⁹ loss rate, microsecond retry).

### Application Buffer Level

| Design | Application HOL? | Mechanism |
|--------|-----------------|-----------|
| **Our Send/Recv** | **No** | Independent recv buffers; consuming buf[5] doesn't depend on buf[3] |
| **Our Ring** | **Yes** | Virtual index slab; ring head advances only in order |
| **msquic Ring** | **Yes** | Ring Head advances only in order; out-of-order releases queue in hash table |
| **rsocket Ring** | **Yes** | Split-buffer halves; must complete current half before cycling to next |

```
Send/Recv — no app-level HOL:
  [buf0: ✓] [buf1: ✗] [buf2: ✓] [buf3: ✓]
  Can process buf0, buf2, buf3 regardless of buf1 state.

Ring buffer — app-level HOL (ours, msquic, rsocket):
  [Pkt_A: slow][Pkt_B: done][Pkt_C: done][  free  ]
   ^Head (stuck)                          ^Tail
  Can't advance Head past slow Pkt_A. Ring fills up. Sender stalls.
```

## Flow Control & Backpressure

| Mechanism | **Our Send/Recv** | **Our Ring** | **msquic** | **rsocket** |
|-----------|-----------|--------|---------|---------|
| **Primary** | RNR retry (hardware) | Credit count (absolute) | Ring buffer space check | Credit-based (RS_OP_SGL) |
| **On exhaustion** | HCA retries with backoff | `drain_recv_credits()` + `Ok(0)` | Enqueue to SendQueue | Block in rsend() |
| **Overhead** | Zero (hardware handles it) | 1 Send+Imm WR per repost | Check + queue + drain per send | Credit messages consume bandwidth |
| **Granularity** | Per-QP (recv buffer count) | Per-message (credit count) | Per-byte (ring free space) | Per-message (sequence numbers) |
| **Self-healing** | N/A (hardware) | ✅ Absolute encoding | No | No |
| **Starvation risk** | Low (HCA exponential backoff) | Low (inline drain + retry) | Send queue can grow unbounded | Can deadlock if both peers block |

### Our Credit Flow in Detail

```
Sender tracks:    remote_credits (starts at max_outstanding = capacity / max_msg_size)
Receiver tracks:  local_freed_credits (monotonic counter)

When receiver calls repost_recv():
  1. Release ring space (recv_ring.release)
  2. Repost doorbell recv WR
  3. Increment local_freed_credits
  4. Post SendWithImm(imm = local_freed_credits)  ← absolute count

Sender receives credit update:
  1. delta = freed_count.wrapping_sub(remote_freed_received)
  2. remote_credits += delta
  3. remote_freed_received = freed_count
  → Self-healing: missed updates recovered by next successful one

On credit exhaustion:
  1. send_copy() calls drain_recv_credits() — non-blocking recv CQ poll
  2. If credits recovered → send proceeds
  3. If still 0 → return Ok(0), AsyncRdmaStream retries after poll_send_completion
```

## Connection Setup Complexity

| Phase | **Our Send/Recv** | **Our Ring** | **msquic** | **rsocket** |
|-------|-----------|--------|---------|---------|
| **RDMA CM connect** | ✓ | ✓ | ✓ | ✓ (transparent) |
| **QP creation** | ✓ | ✓ | ✓ | ✓ |
| **MR registration** | ✓ | ✓ | ✓ | ✓ |
| **MW bind** | — | ✅ (recv ring scoped) | ✓ (RecvMW + OffsetMW) | — |
| **Token exchange** | — | ✓ (20B Send/Recv) | ✓ (24B Send/Recv) | ✓ (CM private data) |
| **Pre-post recv WRs** | ✓ (data bufs) | ✓ (doorbell bufs) | ✓ (doorbells) | — |
| **Credit initialization** | — | ✓ (max_outstanding) | — | ✓ (sseq/rseq init) |
| **iWARP detection** | — | ✓ (reject) | — | ✓ (adapt) |
| **Connection states** | 2 | 3 (setup→token→ready) | 13 | ~8 |

## Advantages of Ring Buffer (Write-Based)

| Advantage | Detail |
|-----------|--------|
| **One fewer copy** | Data lands directly in remote app memory — receiver reads in-place |
| **No recv buffer management** | No pre-posting, tracking, or reposting recv WQEs on the data path |
| **Larger capacity** | Ring can be 64KB-4GB vs RQ depth limit (128-4096 entries) |
| **No RNR risk** | Ring is always "ready" — no missing recv buffers |
| **Batched notification** | One doorbell (Write-With-Immediate) can signal a large transfer |
| **CPU efficiency on receiver** | HCA writes directly to final location — no recv WQE matching |

## Disadvantages of Ring Buffer (Write-Based)

| Disadvantage | Detail |
|--------------|--------|
| **Application-level HOL blocking** | Ring Head must advance in order. One slow packet blocks all. |
| **Security exposure** | Remote peer has write access to your memory via rkey |
| **Token exchange** | Post-connect handshake adds latency and protocol states |
| **Ring protocol complexity** | Head/tail, wrap-around, credit tracking, offset encoding |
| **No iWARP** | Write-With-Immediate not supported on iWARP/siw |
| **Memory pinned upfront** | Entire ring registered for connection lifetime, even if mostly idle |
| **No hardware flow control** | Must track ring space/credits in software |
| **Debugging difficulty** | Data appears in remote memory "magically" — harder to trace |

## Suitability by Workload

| Workload | Best Design | Why |
|----------|------------|-----|
| **Small packets (QUIC, RPC)** | `SendRecvConfig` (Send/Recv) | Minimal overhead; extra copy negligible at 1.2KB |
| **Bulk streaming (large files)** | `CreditRingConfig` (Write+Ring) | One fewer copy matters at high throughput; ring absorbs bursts |
| **Many peers (microservices)** | `SendRecvConfig` (Send/Recv) | No per-peer ring memory; simpler teardown |
| **iWARP / siw / all hardware** | `SendRecvConfig` (Send/Recv) | Works on IB, RoCE, iWARP, siw, rxe without adaptation |
| **Latency-sensitive** | `SendRecvConfig` (Send/Recv) | Fewer protocol states; no token exchange latency |
| **InfiniBand/RoCE bulk** | `CreditRingConfig` (Write+Ring) | Credit flow + ring buffer maximizes NIC pipeline |
| **Memory-constrained** | `SendRecvConfig` (Send/Recv) | Per-connection buffers scale with actual recv count |
| **Drop-in legacy (POSIX)** | rsocket | Socket API compatibility; transparent to applications |

## Summary

```
                Simplicity                              Throughput
                    ◄──────────────────────────────────────►

  Send/Recv (ours)    Ring (ours)      rsocket        msquic RDMA
  ┌──────────────┐  ┌──────────────┐  ┌────────────┐  ┌──────────────────┐
  │ ~500 lines   │  │ ~1300 lines  │  │ ~3400 lines│  │ ~6000 lines      │
  │ 2 states     │  │ 3 states     │  │ ~8 states  │  │ 13 states        │
  │ No ring      │  │ Credit flow  │  │ Credit flow│  │ Ring + MW + hash │
  │ No HOL (app) │  │ HOL (ring)   │  │ HOL (ring) │  │ HOL (ring)       │
  │ 2 copies     │  │ 1 copy       │  │ 1 copy     │  │ 1 copy           │
  │ HW flow ctrl │  │ SW credits   │  │ SW credits │  │ SW ring check    │
  │ All HW       │  │ IB+RoCE      │  │ IB+RoCE+siw│  │ MANA only       │
  └──────────────┘  └──────────────┘  └────────────┘  └──────────────────┘
  Best: gRPC,       Best: QUIC bulk,  Best: legacy    Best: QUIC bulk
  iWARP, many peers IB/RoCE transfer  POSIX apps      (Windows/MANA)
```

Both our transports implement the same `Transport` trait and are selected via `TransportBuilder` —
consumer code (stream, tonic, quinn) is unchanged. All tests run on both transports via generic
test bodies with `_default` and `_ring` variants.

## References

- [docs/design/rdma-transport-layer.md](rdma-transport-layer.md) — Transport trait architecture
- [docs/design/rdma-ring-transport.md](rdma-ring-transport.md) — Ring transport design
- [docs/design/quinn-rdma.md](quinn-rdma.md) — Quinn RDMA integration design
- [docs/background/msquic-rdma.md](../background/msquic-rdma.md) — msquic RDMA transport analysis
- [docs/background/Rsocket.md](../background/Rsocket.md) — rsocket internal implementation analysis
