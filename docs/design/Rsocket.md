# rsocket: RDMA Byte Stream via librdmacm

**Status:** Research  
**Date:** 2026-03-07

## Overview

rsocket is a POSIX-compatible socket API built on RDMA, shipped as part of `librdmacm` (rdma-core). It implements a byte stream using **RDMA Write with Immediate Data** internally, providing `rsend()`/`rrecv()` that mirror standard `send()`/`recv()` semantics.

## Architecture

```
Sender rsend(buf)                    Receiver rrecv(buf)
  ├─ RDMA Write → remote ring buf     ├─ poll for new data
  ├─ Send immediate (doorbell)         ├─ read from ring position
  └─ advance local tail pointer        └─ advance head, send credit
```

**Ring buffers:** Both sides maintain pre-registered ring buffers. The sender writes directly into the receiver's buffer via one-sided RDMA Write — the receiver's CPU is not involved in the data transfer itself.

**Credit-based flow control:** The receiver advertises available credits (free ring slots). The sender blocks when credits are exhausted. Credits are replenished via small Send messages from receiver to sender.

**Immediate data as doorbell:** RDMA Write with Immediate Data generates a receive completion on the remote side, notifying it that new data has arrived without a separate signaling message.

**Zero-copy on receive:** Data lands directly in the pre-registered ring buffer — one fewer copy than Send/Recv, where data must be copied out of the posted receive buffer.

## Availability

rsocket is part of `librdmacm` (rdma-core). Installed on this machine:

| Component | Details |
|-----------|---------|
| Header | `/usr/include/rdma/rsocket.h` (via `librdmacm-dev`) |
| Library | `librdmacm1t64` v50.0 |
| Tools | `rdmacm-utils`: `rstream`, `rping` |
| Rust integration | FFI via `-lrdmacm`, or bindgen on `rsocket.h` |

### Tokio Integration Challenge

rsocket is a synchronous C library that manages its own event loop and internal buffers. Integrating it with tokio's async model would require:

- Bridging rsocket's blocking `rpoll()` with tokio's `AsyncFd`
- Managing rsocket's internal buffer lifecycle alongside Rust ownership
- Significant unsafe FFI surface for a marginal performance gain

This makes it impractical as a drop-in replacement for our `AsyncRdmaStream`.

## Transport Compatibility Testing

### siw (iWARP) — Partial Support

siw implements Software iWARP over standard Ethernet. rsocket's core data path works because siw supports RDMA Write with Immediate Data for basic transfers.

**rstream results (loopback, 100 iterations):**

| Size | Latency (µs/xfer) | Throughput (Gb/s) | Status |
|------|-------------------|-------------------|--------|
| 64B  | 11.74 | 0.04 | ✅ |
| 4KB  | 29.25 | 1.09 | ✅ |
| 64KB | 110.0 | 4.66 | ✅ |
| 1MB  | 795.0 | 10.06 | ✅ |

**rconnect:** ❌ Fails with "Operation not supported" — some advanced rsocket features (reconnection, keepalive) require operations that siw does not implement.

### rxe (Soft-RoCE) — Not Working

rxe (out-of-tree build, kernel 6.17) provides Software RoCE v2. Despite supporting RDMA Write with Immediate Data, rsocket fails over rxe.

**Verification that rxe RDMA verbs work:**

| Tool | Operation | Status |
|------|-----------|--------|
| `rping` | RDMA Read/Write via rdma_cm | ✅ 5/5 pings |
| `ucmatose` | Send/Recv via rdma_cm | ✅ |
| `ibv_rc_pingpong -g 0` | RC verbs (RoCE GID mode) | ✅ 1140 Mbit/s, 57µs/iter |

**rstream results:** ❌ All sizes fail with "Connection reset by peer". The connection establishes but data transfer fails immediately. Tested with default mode and `-T r` (explicit rdma_cm resolve) — same failure.

**Likely cause:** rsocket's internal protocol negotiation or ring buffer setup has a bug or incompatibility with rxe's RoCE v2 implementation. The core RDMA data path (Send/Recv, Read/Write) works fine — the issue is specific to rsocket's higher-level protocol layer over rxe.

### Summary

| Transport | rstream (data) | rconnect (reconnect) | rping (rdma_cm) |
|-----------|---------------|---------------------|-----------------|
| **siw** (iWARP) | ✅ Works | ❌ Op not supported | ✅ Works |
| **rxe** (Soft-RoCE) | ❌ Connection reset | N/A | ✅ Works |
| **InfiniBand HW** | ✅ Expected | ✅ Expected | ✅ Expected |

## Trade-offs vs Send/Recv

| Aspect | Send/Recv (our approach) | rsocket (Write-based) |
|--------|-------------------------|----------------------|
| **Copies per message** | 2 (sender + receiver) | 1 (sender only) |
| **Receiver CPU** | Active (post + process) | Passive (data arrives in-place) |
| **Setup complexity** | Low | High (exchange MR keys, manage ring) |
| **Flow control** | Implicit (buffer count) | Explicit credits (more protocol code) |
| **iWARP support** | ✅ Full | ⚠️ Partial (siw only, rxe broken) |
| **Memory per conn** | 576KB (1 send + 8 recv) | Similar (ring buffers) |
| **Small msg latency** | ~same | Slightly lower |
| **Large msg throughput** | Lower (extra copy) | Higher (zero-copy recv) |

## Why We Don't Use rsocket

1. **Transport incompatibility:** Broken on rxe, partial on siw. Only reliable on real InfiniBand hardware — we need to support all software transports for development and CI
2. **iWARP limitation:** RDMA Write with Immediate Data is an InfiniBand/RoCE feature. Pure iWARP devices may not support it, limiting portability
3. **Tokio integration cost:** rsocket manages its own event loop. Bridging with async Rust would require substantial unsafe FFI with marginal benefit
4. **Marginal gain for RPC:** Our primary workload is gRPC/tonic with small messages (<1KB). The saved receiver-side copy is negligible at these sizes
5. **Complexity:** Credit-based flow control, ring pointer synchronization, and MR key exchange add significant protocol surface vs simple Send/Recv

## References

- [rsocket(7) man page](https://manpages.debian.org/testing/librdmacm-dev/rsocket.7.en.html) — API and design overview
- [rdma-core rsocket source](https://github.com/linux-rdma/rdma-core/tree/master/librdmacm/rsocket.c) — Implementation
- [RB²: Distributed Ring Buffer](https://jokerwyt.github.io/rb2.pdf) — Write-based ring buffer design (academic)
