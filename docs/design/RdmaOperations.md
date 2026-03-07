# RDMA Operations: Performance Analysis

**Status:** Research  
**Date:** 2026-03-07

## Problem

AsyncRdmaStream uses RDMA **Send/Recv** (two-sided) exclusively. Every message incurs two CPU copies (app→send_mr, recv_mr→app) and requires pre-posted receive buffers. Is this the most efficient approach, or could one-sided operations (RDMA Write/Read) deliver better performance?

## RDMA Operation Taxonomy

| Operation | Sides | Remote CPU | Notification | Use Case |
|-----------|-------|-----------|-------------|----------|
| **Send/Recv** | Two | Yes (must pre-post) | Recv completion | Message passing, RPC |
| **Write** | One | No | None | Bulk data push, logs |
| **Write+Imm** | One | Yes (wakes recv WR) | Recv completion | Data push + signal |
| **Read** | One | No | Local completion only | On-demand data pull |
| **CAS/FAA** | One | No | Local completion only | Distributed locking |

## Current Architecture (Send/Recv)

```
Client write(buf)                    Server read(buf)
  ├─ memcpy → send_mr               ├─ poll_recv_cq()
  ├─ post_send()                     ├─ [completion arrives]
  └─ poll_send_cq()                  ├─ memcpy recv_mr → buf
                                     └─ post_recv(mr)  ← re-post
```

**Per-connection resources:** 1×64KB send buffer + 8×64KB recv buffers = 576KB  
**Copies per message:** 2 (sender-side + receiver-side)  
**Completions per message:** 2 (send CQ + recv CQ)

### Strengths

- **Simple**: Maps directly to AsyncRead/AsyncWrite semantics
- **Portable**: Works on all transports (InfiniBand, RoCE, iWARP/siw)
- **No metadata exchange**: No remote MR keys needed after connection setup
- **Natural flow control**: Receiver controls pace by how fast it re-posts buffers
- **Proven pattern**: FaSST (OSDI'16) showed two-sided RDMA scales better than one-sided for RPC workloads at large cluster sizes

### Weaknesses

- **Two copies per message**: Unavoidable with posted buffers
- **Recv buffer pressure**: Must pre-post enough buffers to absorb bursts. iWARP has no RNR retry — missing buffer = fatal disconnect (we use 8 buffers, learned the hard way)
- **Both CPUs involved**: Receiver must actively post and process completions
- **Memory scales linearly**: 576KB × N connections

## Alternative: RDMA Write-Based Stream

The **rsocket** library (part of librdmacm/rdma-core) implements a POSIX-compatible byte stream using RDMA Write internally. This is the most production-proven alternative.

### rsocket Architecture

```
Sender rsend(buf)                    Receiver rrecv(buf)
  ├─ RDMA Write → remote ring buf     ├─ poll for new data
  ├─ Send immediate (doorbell)         ├─ read from ring position
  └─ advance local tail pointer        └─ advance head, send credit
```

Key design elements:
- **Ring buffers**: Both sides maintain registered ring buffers. Sender writes directly into receiver's buffer via RDMA Write
- **Credit-based flow control**: Receiver advertises available credits. Sender blocks when credits exhausted
- **Immediate data as doorbell**: RDMA Write with Immediate notifies receiver of new data arrival
- **Zero-copy on receive**: Data lands directly in pre-registered buffer — one less copy than Send/Recv

### Trade-offs vs Send/Recv

| Aspect | Send/Recv | Write-Based (rsocket) |
|--------|-----------|----------------------|
| **Copies** | 2 per message | 1 (sender only) |
| **Recv CPU** | Active (post + process) | Passive (data arrives in-place) |
| **Setup complexity** | Low | High (exchange MR keys, manage ring) |
| **Flow control** | Implicit (buffer count) | Explicit credits (more protocol code) |
| **iWARP support** | ✅ Full | ⚠️ Write+Imm not supported on iWARP |
| **Memory** | 576KB/conn | Similar (ring buffers) |
| **Latency** | ~same | Slightly lower (fewer completions) |

### Availability

rsocket is part of `librdmacm` (rdma-core). On this machine it's already installed:
- Header: `/usr/include/rdma/rsocket.h` (via `librdmacm-dev`)
- Library: `librdmacm1t64` 50.0
- Tools: `rdmacm-utils` includes `rstream` and `rping` for benchmarking

It could be called from Rust via FFI or by linking against `-lrdmacm`. However, rsocket is a C library that manages its own event loop and buffers — integrating it with tokio's async model would require significant bridging work.

### Why rsocket Isn't Strictly Better

1. **iWARP incompatibility**: RDMA Write with Immediate Data is an InfiniBand/RoCE feature. iWARP does not support it. Our crate targets all transports including siw
2. **Protocol complexity**: Credit management, ring pointer synchronization, and key exchange add significant implementation and debugging surface
3. **Marginal gain for small messages**: The saved recv-side copy matters for large transfers but is negligible for gRPC/RPC payloads (typically <1KB)

## Other Approaches in the Wild

### Demikernel C++ (v0.0, SOSP'21)

Used **Send/Recv** (same as us) with a unique twist:
- Message-based API (scatter-gather push/pop), not byte stream
- Flow control via **one-sided RDMA Write** to update remote send-window counter
- Custom Hoard-RDMA allocator registered all heap memory for zero-copy
- Single shared CQ (works for coroutine model, not for async/tokio)

Key insight: Even Microsoft Research chose Send/Recv as the primary data path, using Write only for out-of-band metadata (window updates).

### FaSST (OSDI'16)

Used **two-sided Send/Recv over Unreliable Datagrams** for key-value store RPCs:
- Achieved 2× throughput vs one-sided RDMA systems at scale
- One-sided operations cause NIC cache thrashing at high connection counts
- Two-sided keeps NIC state simpler, scales better

### eRPC (NSDI'19)

General-purpose datacenter RPC using two-sided messaging:
- Matched or exceeded one-sided RDMA performance for small RPCs
- Software optimizations (batching, prefetching) close the gap

## Recommendation

**Keep Send/Recv for the byte stream.** The current approach is correct for our use case:

1. **Compatibility**: We support iWARP/siw, which rules out Write+Imm-based designs
2. **Simplicity**: Send/Recv maps cleanly to AsyncRead/AsyncWrite without a ring-buffer protocol layer
3. **Use case fit**: Our primary workload is gRPC/tonic (small RPC messages), where the recv-side copy cost is negligible
4. **Academic consensus**: FaSST and eRPC demonstrated that two-sided RDMA is competitive or superior for RPC workloads

### Potential Optimizations (Future)

If profiling shows Send/Recv is the bottleneck:

- **Inline sends**: For messages ≤128 bytes, inline data avoids the send-MR copy entirely. Requires setting `max_inline_data` on QP creation
- **Shared Receive Queue (SRQ)**: Pool recv buffers across many connections, reducing per-connection memory from 512KB to amortized ~64KB
- **Adaptive buffer count**: Start with 2 recv buffers, scale up under burst detection, scale down at idle
- **Write+Imm fast path**: For InfiniBand/RoCE-only deployments, optionally use RDMA Write for the data path (feature-gated, not default)

## References

- [rsocket(7) man page](https://manpages.debian.org/testing/librdmacm-dev/rsocket.7.en.html) — librdmacm byte stream over RDMA Write
- [FaSST: Fast, Scalable Distributed Transactions](https://www.usenix.org/conference/osdi16/technical-sessions/presentation/kalia) — Two-sided RDMA at scale
- [Design Guidelines for High Performance RDMA Systems](https://www.usenix.org/conference/atc16/technical-sessions/presentation/kalia) — USENIX ATC'16
- [Demikernel SOSP'21](https://irenezhang.net/papers/demikernel-sosp21.pdf) — Datapath OS with RDMA LibOS
- [RB²: Distributed Ring Buffer](https://jokerwyt.github.io/rb2.pdf) — Write-based ring buffer design
