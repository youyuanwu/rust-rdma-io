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

## Alternative: RDMA Write-Based Stream (rsocket)

The **rsocket** library (part of librdmacm) implements a byte stream using RDMA Write internally. It saves one memory copy on the receive side but introduces protocol complexity and has limited transport support. See **[Rsocket.md](Rsocket.md)** for full architecture, benchmarks, and compatibility testing.

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

- [FaSST: Fast, Scalable Distributed Transactions](https://www.usenix.org/conference/osdi16/technical-sessions/presentation/kalia) — Two-sided RDMA at scale
- [Design Guidelines for High Performance RDMA Systems](https://www.usenix.org/conference/atc16/technical-sessions/presentation/kalia) — USENIX ATC'16
- [Demikernel SOSP'21](https://irenezhang.net/papers/demikernel-sosp21.pdf) — Datapath OS with RDMA LibOS
- [rsocket analysis](Rsocket.md) — Detailed rsocket architecture, benchmarks, and compatibility
