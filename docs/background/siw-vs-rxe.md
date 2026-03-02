# siw vs rxe — Software RDMA Providers

Both are Linux kernel modules that provide RDMA over standard Ethernet NICs (no special hardware needed). They implement different RDMA transport protocols.

## Overview

| | **siw** (Soft-iWARP) | **rxe** (Soft-RoCE) |
|---|---|---|
| Protocol | iWARP (RDMA over TCP) | RoCE v2 (RDMA over UDP) |
| Transport | TCP/IP (reliable, ordered) | UDP/IP + IB transport layer |
| Connection model | Stream-oriented (TCP) | IB-style RC/UD |
| Kernel module | `siw` | `rdma_rxe` |

## Protocol Differences

### iWARP (siw)

iWARP layers RDMA on top of TCP. The kernel's TCP stack handles reliability, ordering, congestion control, and retransmission. This makes it straightforward but adds TCP overhead (headers, Nagle, slow-start).

- Connection setup uses TCP (via rdma_cm mapping to TCP sockets)
- Inherits TCP's reliability guarantees
- No link-layer or IP multicast support
- Works across routed L3 networks out of the box

### RoCE v2 (rxe)

RoCE (RDMA over Converged Ethernet) places the InfiniBand transport layer directly over UDP/IP. The IB transport handles reliability (ack/nak, retransmission, sequence numbers) independently of TCP.

- Connection setup uses IB-style CM messages carried over UDP
- Implements its own reliability at the IB transport layer
- Supports more IB features (see verb support below)
- Originally required lossless Ethernet (PFC/ECN); RoCE v2 over UDP relaxes this

## Verb Support

| Verb | siw | rxe |
|---|---|---|
| SEND / RECV | ✅ | ✅ |
| RDMA READ | ✅ | ✅ |
| RDMA WRITE | ✅ | ✅ |
| RDMA WRITE with Immediate | ❌ | ✅ |
| Atomic CAS / FAA | ❌ (reports `ATOMIC_NONE`) | ✅ |
| Memory Windows | ❌ | ❌ |

siw's iWARP protocol does not define RDMA WRITE with Immediate Data or atomic operations — these are InfiniBand-only verbs that rxe supports because it implements the full IB transport.

## Behavioral Differences

### Completion Batching

rxe tends to batch completions more aggressively. A single `ibv_poll_cq` call may return both send and recv completions together, whereas siw more often returns them separately. Code that processes only the first completion in a batch and discards the rest will lose completions on rxe.

### Disconnect Handling

On siw, disconnecting one side (via `rdma_disconnect`) reliably flushes outstanding work requests on the remote QP with `IBV_WC_WR_FLUSH_ERR`. On rxe, the flush may be delayed or not occur until the QP is explicitly destroyed. Code should not rely on `WR_FLUSH_ERR` as a disconnect signal.

### Loopback

siw on loopback (`127.0.0.1` / `siw_lo`) fails `rdma_listen` with `EADDRINUSE`. Use `0.0.0.0` for server bind and the interface IP (e.g., eth0) for client connect. rxe does not have this limitation.

### Performance & Timing

siw (TCP-backed) has higher per-message latency from TCP framing. rxe (UDP-backed) is lower latency but more sensitive to packet loss. On CPU-constrained CI runners, rxe is more likely to expose timing-dependent bugs (spin-poll starvation, completion races).

## Configuration

```sh
# Load siw (binds to all network interfaces automatically)
sudo modprobe siw

# Load rxe and create a device on an interface
sudo modprobe rdma_rxe
sudo rdma link add rxe_eth0 type rxe netdev eth0

# Verify
rdma link
ibv_devices
```

## When to Use Which

- **Local development / CI**: siw is simpler — just `modprobe siw` and it works. Fewer timing surprises.
- **CI robustness testing**: rxe exercises more IB-like behavior (completion batching, atomics, immediate data). Catches bugs that siw's TCP reliability masks.
- **Production**: Neither — use hardware RDMA (ConnectX, etc.). These software providers are for development and testing only.
