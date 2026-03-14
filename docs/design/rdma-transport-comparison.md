# RDMA Transport Design Comparison

**Status:** Research  
**Date:** 2026-03-14  
**Scope:** Three RDMA transport designs compared: our Send/Recv, msquic's Write+Ring, rsocket's Write+Ring

## Overview

Three production/proposed RDMA transports use fundamentally different approaches to deliver
data over RDMA. This document compares their architectures, trade-offs, and suitability for
different workloads.

| | **rust-rdma-io (ours)** | **msquic RDMA (PR #5113)** | **rsocket (librdmacm)** |
|---|---|---|---|
| **Purpose** | Byte stream (gRPC) + Datagram (QUIC) | QUIC packet delivery | POSIX socket drop-in |
| **Platform** | Linux (ibverbs / rdma_cm) | Windows (NDSPI / MANA) | Linux (ibverbs / rdma_cm) |
| **Code size** | ~600 lines (stream) / ~800 est (datagram) | ~6000 lines | ~3400 lines |
| **Language** | Rust (async) | C (IOCP callbacks) | C (blocking + rpoll) |

## Data Transfer Verb

The most fundamental design choice: how data moves between peers.

### Send/Recv (Two-Sided) — Our Approach

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

### RDMA Write + Ring Buffer (One-Sided) — msquic & rsocket

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

| Aspect | Send/Recv (ours) | RDMA Write + Ring (msquic/rsocket) |
|--------|------------------|------------------------------------|
| **Copies** | 2 (sender → send MR, HCA → recv buf) | 1 (sender → send ring, HCA → remote ring in-place) |
| **Receiver CPU** | HCA must match send to posted recv | Zero — data lands directly in app memory |
| **Pre-posted buffers** | Required (N recv WQEs) | Not needed for data path |
| **Flow control** | Hardware RNR retry (automatic) | Application-level ring space tracking |
| **Notification** | Implicit (recv completion = data) | Explicit doorbell (Immediate Data) |
| **Setup** | None beyond QP creation | Token exchange (VA + rkey + capacity) |
| **Memory exposure** | None | Remote peer can write to your ring |

## Ring Buffer Protocol

msquic and rsocket both use ring buffers but with different details.

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

### Our Approach: No Ring Buffer

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

### Wire Level (RC QP — All Three Designs)

All three use RC (Reliable Connection) QPs, which provide TCP-like in-order delivery:

```
Sender posts: A → B → C
Packet B lost on wire:
  HCA holds C, retransmits B (invisible to software)
  Application sees: A ... [wait] ... B, C
```

This is inherent to RC and affects all three equally. Practical impact is negligible in
datacenter networks (<10⁻⁹ loss rate, microsecond retry).

### Application Buffer Level

| Design | Application HOL? | Mechanism |
|--------|-----------------|-----------|
| **Our Send/Recv** | **No** | Independent recv buffers; consuming buf[5] doesn't depend on buf[3] |
| **msquic Ring** | **Yes** | Ring Head advances only in order; out-of-order releases queue in hash table |
| **rsocket Ring** | **Yes** | Split-buffer halves; must complete current half before cycling to next |

```
Our design — no app-level HOL:
  [buf0: ✓] [buf1: ✗] [buf2: ✓] [buf3: ✓]
  Can process buf0, buf2, buf3 regardless of buf1 state.

Ring buffer — app-level HOL:
  [Pkt_A: slow][Pkt_B: done][Pkt_C: done][  free  ]
   ^Head (stuck)                          ^Tail
  Can't advance Head past slow Pkt_A. Ring fills up. Sender stalls.
```

## Flow Control & Backpressure

| Mechanism | Our Design | msquic | rsocket |
|-----------|-----------|--------|---------|
| **Primary** | RNR retry (hardware) | Ring buffer space check | Credit-based (RS_OP_SGL) |
| **On exhaustion** | HCA retries with backoff | Enqueue to SendQueue | Block in rsend() |
| **Overhead** | Zero (hardware handles it) | Check + queue + drain per send | Credit messages consume bandwidth |
| **Granularity** | Per-QP (recv buffer count) | Per-byte (ring free space) | Per-message (sequence numbers) |
| **Starvation risk** | Low (HCA exponential backoff) | Send queue can grow unbounded | Can deadlock if both peers block |

### rsocket Credit Flow in Detail

```
Sender tracks:    sseq_no (next send seq), sseq_comp (last ack'd)
Receiver tracks:  rseq_no (next expected), rseq_comp (last consumed)

When receiver consumes data:
  1. Update rseq_comp
  2. If enough credits freed: RDMA Write RS_OP_SGL to sender's remote_sgl
     (tells sender: "you can send up to sseq_comp + window")

Sender before each send:
  1. Check: sseq_no < sseq_comp + sq_size?
  2. If yes: send
  3. If no: block (poll CQ for credits or call rpoll)
```

This is the most sophisticated flow control but also the most complex — explicit credit
messages add latency and can deadlock if both peers exhaust credits simultaneously.

## Connection Setup Complexity

| Phase | Our Design | msquic | rsocket |
|-------|-----------|--------|---------|
| **RDMA CM connect** | ✓ | ✓ | ✓ (transparent) |
| **QP creation** | ✓ | ✓ | ✓ |
| **MR registration** | ✓ | ✓ | ✓ |
| **MW bind** | — | ✓ (RecvMW + OffsetMW) | — |
| **Token exchange** | — | ✓ (Send/Recv of private data) | ✓ (CM private data) |
| **Pre-post recv buffers** | ✓ | ✓ (for doorbells only) | — |
| **Credit initialization** | — | — | ✓ (sseq/rseq init) |
| **iWARP detection** | — | — | ✓ (IBV_TRANSPORT_IWARP check) |
| **Connection states** | 2 | 13 | ~8 (hidden in rsocket) |

## Advantages of Ring Buffer (Write-Based)

| Advantage | Detail |
|-----------|--------|
| **One fewer copy** | Data lands directly in remote app memory — receiver reads in-place |
| **No recv buffer management** | No pre-posting, tracking, or reposting recv WQEs on the data path |
| **Larger capacity** | Ring can be 64KB-4GB vs RQ depth limit (128-4096 entries) |
| **No RNR risk** | Ring is always "ready" — no missing recv buffers |
| **Batched notification** | One doorbell (Write-With-Immediate) can signal a large transfer |
| **CPU efficiency on receiver** | HCA writes directly to final location — no recv WQE matching |
| **Overlapped sends (rsocket)** | Multiple in-flight writes to different ring offsets |

## Disadvantages of Ring Buffer (Write-Based)

| Disadvantage | Detail |
|--------------|--------|
| **Application-level HOL blocking** | Ring Head must advance in order. One slow packet blocks all. |
| **Security exposure** | Remote peer has write access to your memory via rkey |
| **MW lifecycle (msquic)** | Must bind per-connection, invalidate on disconnect |
| **Token exchange** | Post-connect handshake adds latency and protocol states |
| **Ring protocol complexity** | Head/tail, wrap-around, completion hash tables, offset sync (~500 lines) |
| **13-state machine (msquic)** | Each state has error paths, recovery, and teardown logic |
| **Credit protocol (rsocket)** | Explicit credit messages consume bandwidth, risk deadlock |
| **Memory pinned upfront** | Entire ring registered for connection lifetime, even if mostly idle |
| **No hardware flow control** | Must track ring space in software; RNR retry not available |
| **Debugging difficulty** | Data appears in remote memory "magically" — harder to trace |
| **Fragile to bugs** | Wrap-around off-by-one, completion table leak, head/tail desync → silent corruption |
| **iWARP adaptation (rsocket)** | iWARP doesn't support Write-With-Immediate; needs 2 WRs per message |

## Suitability by Workload

| Workload | Best Design | Why |
|----------|------------|-----|
| **Small packets (QUIC, RPC)** | Send/Recv | Minimal overhead per packet; extra copy is negligible at 1.2KB |
| **Bulk streaming (large files)** | Write + Ring | One fewer copy matters at high throughput; ring absorbs bursts |
| **Many peers (microservices)** | Send/Recv | No per-peer ring memory; simpler teardown |
| **Drop-in replacement (legacy)** | rsocket | POSIX socket API compatibility; transparent to applications |
| **Latency-sensitive** | Send/Recv | Fewer protocol states; no token exchange latency |
| **Hardware diversity** | Send/Recv | Works on IB, RoCE, iWARP, siw, rxe without adaptation |
| **Memory-constrained** | Send/Recv | Per-connection buffers scale with actual recv count, not ring capacity |

## Summary

```
                Simplicity                              Throughput
                    ◄──────────────────────────────────────►

  Send/Recv (ours)          rsocket              msquic RDMA
  ┌──────────────┐    ┌──────────────┐    ┌──────────────────┐
  │ ~600 lines   │    │ ~3400 lines  │    │ ~6000 lines      │
  │ 2 states     │    │ ~8 states    │    │ 13 states        │
  │ No ring      │    │ Credit flow  │    │ Ring + MW + hash │
  │ No HOL (app) │    │ HOL (ring)   │    │ HOL (ring)       │
  │ 2 copies     │    │ 1 copy       │    │ 1 copy           │
  │ HW flow ctrl │    │ SW credits   │    │ SW ring check    │
  │ All HW       │    │ IB+RoCE(+siw)│    │ MANA only        │
  └──────────────┘    └──────────────┘    └──────────────────┘
  Best: QUIC, RPC     Best: legacy apps   Best: QUIC bulk xfer
```

For our quinn-rdma use case (QUIC packets, ~1.2KB, datacenter), Send/Recv is the clear choice:
the ring buffer's throughput advantage is negligible at small packet sizes, while its complexity
cost is substantial.

## References

- [docs/design/quinn-rdma.md](quinn-rdma.md) — Quinn RDMA integration design
- [docs/background/msquic-rdma.md](../background/msquic-rdma.md) — msquic RDMA transport analysis
- [docs/background/Rsocket.md](../background/Rsocket.md) — rsocket internal implementation analysis
