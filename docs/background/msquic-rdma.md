# MsQuic RDMA Transport (PR #5113)

**Status:** Research  
**Date:** 2026-03-07  
**Source:** [microsoft/msquic#5113](https://github.com/microsoft/msquic/pull/5113) (branch: `rdma-ndspi`)

## Overview

MsQuic PR #5113 adds an RDMA datapath transport alongside the existing UDP/TCP paths. It implements QUIC packet delivery over RDMA using **RDMA Write into remote ring buffers** — the same fundamental pattern as rsocket, but built on Windows **NetworkDirect SPI (NDSPI)** with **MANA (Microsoft Azure Network Adapter)** extensions.

The implementation is ~6000 lines of C in `datapath_winrdma.c` plus a ring buffer library.

## Architecture

### Layer Stack

```
  QUIC Protocol (src/core)
       │
  Datapath Abstraction (quic_datapath.h)
       │
  ┌────┴────────────────────┐
  │  datapath_winrdma.c     │  ← NEW: RDMA datapath
  │  NDSPI / MANA CQ/QP    │
  └────┬────────────────────┘
       │
  NetworkDirect Provider (kernel)
       │
  MANA / ConnectX / Any ND2 HW
```

### Data Path: RDMA Write + Ring Buffers

The core data transfer uses **one-sided RDMA Write** into pre-registered ring buffers on the remote peer. This is the same design as rsocket.

```
Sender (CxPlatRdmaSend)                   Receiver
  ├─ memcpy data → local SendRingBuffer
  ├─ RDMA Write → remote RecvRingBuffer    (data lands in remote memory)
  ├─ Send w/ Immediate Data (doorbell)     ├─ RecvCQ completion fires
  └─ SendCQ completion → release buffer    ├─ read from RecvRingBuffer[offset]
                                           └─ release recv buffer, advance head
```

**Ring buffers:**
- `RDMA_SEND_RING_BUFFER` — local staging area. Sender copies data here, then writes to remote.
- `RDMA_RECV_RING_BUFFER` — local landing zone. Remote peer writes into this via RDMA Write.
- `RDMA_REMOTE_RING_BUFFER` — tracks the remote peer's recv ring: base address, rkey, head/tail pointers.

Head/tail wrap-around and out-of-order completion release are tracked via a hashtable (`SendCompletionTable` / `RecvCompletionTable`).

### Connection Establishment

Connection setup follows a multi-phase state machine:

```
  Uninitialized
    → RingBufferRegistered    (register local MR, bind Memory Windows)
    → Connecting              (ND2 Connector.Connect / Listen+Accept)
    → CompleteConnect         (three-way handshake done)
    → TokenExchangeInitiated  (exchange MR tokens via Send/Recv)
    → TokenExchangeComplete   (both peers have remote ring buffer info)
    → Ready                   (data transfer begins)
    → ReceivedDisconnect / Closing / Closed
```

**Token exchange** happens after the RDMA connection is established. Each peer sends a packed `RDMA_DATAPATH_PRIVATE_DATA` struct containing:
- `RemoteAddress` — base virtual address of the recv ring buffer
- `Capacity` — ring buffer size
- `RemoteToken` — memory window rkey for RDMA Write access
- `RemoteOffsetBufferAddress` + `Token` — for offset synchronization

Memory Windows (`IND2MemoryWindow`) are used for security — they scope the remote rkey to exactly the recv ring buffer region, and can be invalidated on disconnect.

### Completion Queue Design

Uses **separate send and recv CQs** (dual-CQ, same as our `AsyncQp` design):

```c
typedef struct _RDMA_NDSPI_CONNECTION {
    IND2ManaCompletionQueue*    RecvCompletionQueue;
    IND2ManaCompletionQueue*    SendCompletionQueue;
    IND2ManaQueuePair*          QueuePair;
    // ...
};
```

The CQs use the MANA-extended `IND2ManaCompletionQueue` interface, which returns `ND2_MANA_RESULT` including the immediate data value from Write-with-Immediate or Send-with-Immediate operations.

### MANA Extensions

The PR defines MANA-specific extensions to the standard ND2 interfaces:

| Interface | Extends | Added Methods |
|-----------|---------|---------------|
| `IND2ManaCompletionQueue` | `IND2CompletionQueue` | `GetManaResults()` — returns `ND2_MANA_RESULT` with ImmediateData |
| `IND2ManaQueuePair` | `IND2QueuePair` | `SendWithImmediate()`, `WriteWithImmediate()` |

`ND2_MANA_RESULT` includes a `RequestType` enum that distinguishes:
- `Nd2ManaRequestTypeSend`, `Read`, `Write`, `Recv`
- `Nd2ManaRequestTypeRecvWithImmediate`, `RecvRdmaWithImmediate`

### RDMA Operations Used

| Operation | Purpose |
|-----------|---------|
| **RDMA Write** | Bulk data transfer into remote recv ring buffer |
| **Write with Immediate** | Data transfer + doorbell notification |
| **Send with Immediate** | Doorbell / ring buffer offset updates |
| **Send/Recv** | Control plane: token exchange during connection setup |
| **RDMA Read** | Reading remote ring buffer offset state |
| **Bind/Invalidate** | Memory Window lifecycle management |

## Integration into MsQuic

### New APIs

```c
// Execution config flag to enable RDMA
QUIC_EXECUTION_CONFIG_FLAG_RDMA = 0x0040

// RDMA-specific socket creation
CxPlatSocketCreateRdma(Datapath, LocalAddr, RemoteAddr, Context, Config, &Socket);
CxPlatSocketCreateRdmaListener(Datapath, LocalAddr, Context, Config, &Socket);

// RDMA adapter lifecycle
CxPlatRdmaAdapterInitialize(LocalAddress, &Adapter);
CxPlatRdmaAdapterRelease(Adapter);
```

### RDMA Config

```c
typedef struct CXPLAT_RDMA_CONFIG {
    uint32_t Flags;                  // SHARE_ENDPOINT, SHARE_MR, SHARE_CQ, NO_MEMORY_WINDOW
    uint32_t SendRingBufferSize;
    uint32_t RecvRingBufferSize;
    unsigned long PostReceiveCount;   // pre-posted recv buffers for doorbell messages
    // ... processor affinity, CIBIR ID for demuxing
};
```

### Callback Structure

```c
typedef struct CXPLAT_RDMA_DATAPATH_CALLBACKS {
    Accept;        // new connection arrived
    Connect;       // connection established / disconnected
    Receive;       // data available in recv ring buffer
    SendComplete;  // send buffer can be released
};
```

## Comparison with rsocket

Both msquic-RDMA and rsocket use RDMA Write into remote ring buffers for data transfer. They share the same fundamental design but differ in API layer, platform, and protocol details.

| Aspect | msquic RDMA (PR #5113) | rsocket (librdmacm) |
|--------|----------------------|---------------------|
| **Platform** | Windows (NDSPI / MANA) | Linux (ibverbs / rdma_cm) |
| **API style** | Async callbacks (IOCP overlapped) | POSIX blocking (`rsend`/`rrecv`/`rpoll`) |
| **Ring buffer mgmt** | Custom (`datapath_rdma_ringbuffer.c`) | Built into librdmacm |
| **Doorbell mechanism** | Send/Write with Immediate Data | Write with Immediate Data |
| **Completion queue** | Dual CQ (MANA extended) | Single shared CQ |
| **Memory protection** | Memory Windows (bind/invalidate per-conn) | Memory Regions only |
| **Token exchange** | Post-connect handshake (private data struct) | Built into rsocket protocol |
| **Offset sync** | RDMA Read of remote offset buffer | Implicit in rsocket protocol |
| **Connection setup** | 7+ state machine phases | Transparent (mirrors TCP `connect`/`accept`) |
| **Reconnection** | Not implemented | `rconnect` (partially supported) |
| **Code complexity** | ~6000 lines (new code) | ~4000 lines (in librdmacm) |
| **Transport compat** | MANA hardware only | IB + RoCE (broken on rxe, partial on siw) |
| **Integration target** | QUIC protocol (structured packets) | Generic byte stream (POSIX socket drop-in) |

**Key differences:**

1. **Memory Windows vs Memory Regions**: msquic uses MW (Type 2) to scope remote write access to exactly the recv ring buffer, with per-connection invalidation on disconnect. rsocket uses only MR-level protection — the entire registered region is exposed for the connection lifetime.

2. **Offset synchronization**: msquic explicitly reads remote ring buffer offsets via RDMA Read to track the peer's head pointer. rsocket embeds offset updates in its internal credit protocol via Send messages.

3. **Dual vs single CQ**: msquic uses separate send/recv CQs (avoiding the mixed-completion polling problem we also solved). rsocket uses a single shared CQ.

4. **Protocol transparency**: rsocket hides the RDMA ring buffer protocol behind standard socket APIs — applications call `rsend()`/`rrecv()` and the library handles everything. msquic exposes the ring buffer model to its datapath layer, requiring explicit buffer reservation/release.

## Comparison with Our Architecture

| Aspect | msquic RDMA (PR #5113) | rust-rdma-io (AsyncRdmaStream) |
|--------|----------------------|-------------------------------|
| **RDMA API** | Windows NDSPI / MANA | Linux ibverbs / rdma_cm |
| **Data transfer** | RDMA Write into remote ring buffer | Send/Recv (two-sided) |
| **Notification** | Send/Write with Immediate Data | CQ completions on recv |
| **CQ design** | Dual CQ (send + recv) | Dual CQ (send + recv) ✓ same |
| **Buffer model** | Ring buffers (head/tail, wrap-around) | Fixed send + pre-posted recv buffers |
| **Flow control** | Credit-based (ring buffer space) | Implicit (recv buffer count) |
| **Security** | Memory Windows (bind/invalidate) | Not needed (two-sided = no remote access) |
| **Token exchange** | Required (post-connect handshake) | Not needed |
| **Copies per msg** | 1 (sender only, data lands in-place) | 2 (sender + receiver) |
| **Portability** | Windows + MANA hardware only | All transports (IB, RoCE, iWARP/siw) |
| **Protocol complexity** | High (~6000 lines) | Low (~600 lines) |

## Key Takeaways

1. **RDMA Write is the Microsoft choice** for QUIC-over-RDMA data transfer — validates that Write-based streams are production-viable when hardware supports it

2. **Dual CQ is standard** — msquic also uses separate send/recv CQs, confirming our design decision

3. **Token exchange adds complexity** — the multi-phase connection state machine (7+ states) is necessary for Write-based designs but absent from Send/Recv designs

4. **Memory Windows for security** — Write-based designs expose remote memory, requiring MW bind/invalidate lifecycle management. Send/Recv avoids this entirely

5. **Hardware-specific** — MANA extensions (`IND2ManaQueuePair`, `IND2ManaCompletionQueue`) tie this to Azure NIC hardware. Our ibverbs approach is portable across all RDMA transports

6. **Ring buffer complexity** — the `datapath_rdma_ringbuffer.c` handles wrap-around, out-of-order completions, and head/tail synchronization — significant protocol surface absent from two-sided designs

## References

- [microsoft/msquic#5113](https://github.com/microsoft/msquic/pull/5113) — RDMA NDSPI datapath PR
- [NetworkDirect SPI](https://learn.microsoft.com/en-us/windows-hardware/drivers/network/overview-of-network-direct-kernel-provider-interface--ndkpi-) — Windows RDMA abstraction
- [MANA (Microsoft Azure Network Adapter)](https://learn.microsoft.com/en-us/azure/virtual-network/accelerated-networking-mana-overview) — Azure SmartNIC
