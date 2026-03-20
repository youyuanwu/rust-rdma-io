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

## QUIC-to-RDMA Connection Mapping

### msquic Approach: 1 QUIC connection = 1 RDMA connection

MsQuic replaces the UDP socket with an RDMA connection at the **datapath** layer.
The mapping is **1:1** — each QUIC connection (identified by a remote address) gets
its own dedicated RDMA connection with its own QP, CQs, and ring buffers:

```
  QUIC Connection A (peer 10.0.0.1:4433)
    └── RDMA_CONNECTION A  (QP, send/recv CQ, ring buffers)

  QUIC Connection B (peer 10.0.0.2:4433)
    └── RDMA_CONNECTION B  (QP, send/recv CQ, ring buffers)
```

The `CXPLAT_SOCKET` struct has a `RdmaContext` pointer to an `RDMA_CONNECTION`:

```c
Socket->RdmaContext = RdmaConnection;   // 1:1 binding
RdmaConnection->Socket = Socket;        // back-pointer
```

**QUIC streams are invisible to the RDMA layer.** The QUIC core multiplexes streams
into QUIC packets (datagrams), and each datagram is delivered as a single
`WriteWithImmediate` into the remote ring buffer. The RDMA layer sees opaque packets,
not streams.

**Connection lifecycle:** msquic creates a new `RDMA_CONNECTION` (with its own QP +
CQs + ring buffers) per QUIC connection via `SocketCreateRdmaInternal()`. The RDMA
connection goes through the 13-state machine (connect → token exchange → ready).
On the server side, `SocketCreateRdmaListener()` creates a listener, and
`GetConnectionRequest` + `Accept` create per-client RDMA connections.

**CIBIR (Connection ID Based Internal Routing):** For shared endpoints (multiple
QUIC connections on the same IP:port), msquic uses CIBIR IDs in the RDMA connection's
private data to demux incoming connections to the correct QUIC connection.

### Our Approach (rdma-io-quinn): Same Architecture

Our `RdmaUdpSocket<B>` (in `rdma-io-quinn`) takes the **same 1:1 approach**:

```
  Quinn Endpoint
    └── RdmaUdpSocket<B: TransportBuilder>
          ├── AsyncCmListener (accepts new peers)
          └── HashMap<SocketAddr, Arc<Mutex<B::Transport>>>
                ├── 10.0.0.1:4433 → CreditRingTransport A  (QP, CQs, ring)
                └── 10.0.0.2:4433 → CreditRingTransport B  (QP, CQs, ring)
```

`RdmaUdpSocket` implements Quinn's `AsyncUdpSocket` trait — the same abstraction
boundary as msquic's `CXPLAT_SOCKET`. Both replace the UDP datagram path while the
QUIC protocol stack above runs unchanged.

### Side-by-Side Comparison

| Aspect | msquic | rdma-io-quinn |
|--------|--------|---------------|
| **Abstraction boundary** | `CXPLAT_SOCKET` (datapath layer) | `AsyncUdpSocket` (Quinn trait) |
| **Mapping** | 1 QUIC connection = 1 `RDMA_CONNECTION` | 1 SocketAddr = 1 `B::Transport` |
| **Connection registry** | `Socket->RdmaContext` (1:1 pointer) | `HashMap<SocketAddr, Arc<Mutex<T>>>` (N peers) |
| **QUIC streams** | Invisible — RDMA sees opaque packets | Invisible — same |
| **Server accept** | `Listener → GetConnectionRequest → Accept` | `AsyncCmListener → poll_accept → insert into map` |
| **Client connect** | `Connector.Connect(RemoteAddr)` | `connect_to(addr)` pre-establishes RDMA |
| **Multiplexing** | 1 QP per connection (dedicated) | 1 QP per connection (dedicated) |
| **Send dispatch** | `RdmaSocketSend(Socket, Route, SendData)` | `try_send(transmit)` → lookup by `transmit.destination` |
| **Recv dispatch** | `RecvCQ fires → upcall(Socket, RecvData)` | `poll_recv → iterate all transports → copy to Quinn bufs` |
| **Data unit** | QUIC packet as single `WriteWithImmediate` | QUIC packet as single `send_copy` |
| **Backpressure** | `SendQueue` — queues pending sends | `WouldBlock` — Quinn retries via `poll_writable` |
| **Multi-connection polling** | 1 IOCP per connection (event-driven) | Round-robin all transports in `poll_recv` |

### Key Similarities

1. **Same 1:1 model**: Both give each QUIC peer its own RDMA connection (QP + CQs + buffers).
   There is no RDMA-level multiplexing of multiple peers onto one QP.

2. **Datapath replacement**: Both replace the UDP socket layer. The QUIC protocol
   (TLS, congestion control, loss recovery, stream multiplexing) runs unmodified above.

3. **Opaque packets**: RDMA carries QUIC packets as discrete datagrams. No stream
   awareness at the RDMA layer.

4. **Point-to-point connection**: Both require an RDMA connection (CM handshake +
   token exchange) before data transfer. msquic does this in `NdspiConnect` / `NdspiAccept`;
   we do it in `connect_to()` / `poll_accept()`.

### Key Differences

1. **Accept model**: msquic's server creates each client's full RDMA connection
   inside the accept handler (`SocketCreateRdmaInternal` with `RDMA_SERVER` type).
   Our server spawns an async `accept` future from the `TransportBuilder`, which
   runs in the background and inserts the transport into the connection map when done.

2. **Recv polling**: msquic uses IOCP — each connection's RecvCQ independently
   fires a completion event. Our `poll_recv` iterates all connections in round-robin,
   which is simpler but O(N) in connection count. For large connection counts, an
   epoll-based approach (one `AsyncFd` per recv CQ) would be more efficient.

3. **Backpressure**: msquic queues sends internally (`SendQueue`) and drains them
   on send CQ completion. We return `WouldBlock` and rely on Quinn's `poll_writable` →
   `RdmaUdpPoller` which waits on the blocked transport's send CQ.

4. **Shared endpoints / CIBIR**: msquic supports multiplexing multiple QUIC
   connections onto a shared RDMA endpoint via CIBIR ID routing. We don't support
   this — each connection has its own listener port (point-to-point).

## Internal Implementation Details

*Source: ~6000 lines in `datapath_winrdma.c` + ~500 lines in `datapath_rdma_ringbuffer.c`*

### How QUIC Maps onto RDMA

MsQuic treats the RDMA transport as a **datapath** — the same abstraction layer used for UDP sockets.
The QUIC protocol stack (connection management, TLS, stream multiplexing, congestion control, loss recovery)
runs unchanged on top. Only the packet delivery mechanism changes:

```
  QUIC Core (connections, streams, TLS, CC, loss recovery)
       │
       │ CxPlatSocketSend(packet)        CxPlatRecvData callback(packet)
       ▼                                        ▲
  ┌─────────────────────────────────────────────────────┐
  │              datapath_winrdma.c                      │
  │  RdmaSocketSend()                                   │
  │    1. Reserve slot in remote recv ring buffer        │
  │    2. memcpy QUIC packet → local send ring buffer    │
  │    3. RDMA Write With Immediate → remote ring buffer │
  │    4. SendCQ completion → release send ring slot     │
  │                                                      │
  │  RecvCQ completion fires (Write-With-Imm received):  │
  │    1. Parse immediate data → offset + length         │
  │    2. Point CXPLAT_RECV_DATA at recv ring[offset]    │
  │    3. Upcall → QUIC Core processes QUIC packet       │
  │    4. Release recv ring slot, re-post receive         │
  └─────────────────────────────────────────────────────┘
```

**Key insight:** Each QUIC packet (including headers, crypto frames, stream frames) is delivered
as a single RDMA Write With Immediate. The QUIC core sees it identically to a UDP datagram arrival.
There is no byte-stream abstraction — RDMA delivers discrete packets, which is a natural fit for QUIC's
packet-oriented design.

### Core Data Structures

```c
// Per-connection state (~6000 lines of lifecycle management)
typedef struct _RDMA_NDSPI_CONNECTION {
    RDMA_NDSPI_ADAPTER*         Adapter;
    HANDLE                      ConnOverlappedFile;   // IOCP handle
    IND2MemoryRegion*           MemoryRegion;         // Single MR for all buffers
    IND2MemoryWindow*           RecvMemoryWindow;     // MW scoping recv ring
    IND2MemoryWindow*           OffsetMemoryWindow;   // MW scoping offset buffer
    IND2ManaCompletionQueue*    RecvCompletionQueue;   // Dual CQ
    IND2ManaCompletionQueue*    SendCompletionQueue;
    IND2ManaQueuePair*          QueuePair;
    IND2Connector*              Connector;
    RDMA_SEND_RING_BUFFER*      SendRingBuffer;       // Local staging area
    RDMA_RECV_RING_BUFFER*      RecvRingBuffer;       // Landing zone for remote writes
    RDMA_REMOTE_RING_BUFFER*    RemoteRingBuffer;     // Tracks remote peer's recv ring
    RDMA_CONNECTION_STATE       State;                 // 13-state machine
    CXPLAT_LIST_ENTRY           SendQueue;             // Pending sends (backpressure)
    ULONG                       Flags;                 // MW_USED, OFFSET_USED, etc.
} RDMA_CONNECTION;

// Token exchanged during connection handshake (packed, 24 bytes)
typedef struct _RDMA_DATAPATH_PRIVATE_DATA {
    uint64_t RemoteAddress;           // Base VA of recv ring buffer
    uint32_t Capacity;                // Ring buffer size
    uint32_t RemoteToken;             // MW rkey for RDMA Write
    uint64_t RemoteOffsetBufferAddress;  // Offset sync buffer VA
    uint32_t RemoteOffsetBufferToken;    // Offset sync rkey
} RDMA_DATAPATH_PRIVATE_DATA;
```

### Connection State Machine

```
  Uninitialized
      │ (alloc MR, register memory)
      ▼
  RingBufferRegistered
      │ (Connector.Connect / Listener.GetConnectionRequest)
      ▼
  Connecting ◄─────────────── WaitingForGetConnRequest (server)
      │ (Connect completes)        │ (GetConnectionRequest completes)
      ▼                            ▼
  CompleteConnect              WaitingForAccept
      │ (CompleteConnect           │ (Accept completes)
      │  IOCP completes)           │
      ▼                            ▼
  Connected ◄──────────────── Connected
      │ (Bind MW, post recv, exchange tokens via Send/Recv)
      ▼
  TokenExchangeInitiated
      │ (Recv CQ fires: got peer's ring buffer info)
      │ (Send own ring buffer info to peer)
      ▼
  TokenExchangeComplete
      │ (Verify both sides have remote ring info)
      ▼
  Ready ─────────────────────── Data transfer begins
      │
  ReceivedDisconnect / Closing / Closed
```

**13 states total.** The critical path is `Ready` — only in this state can data transfer occur.
States before `Ready` handle resource allocation, RDMA connection, and ring buffer token exchange.

### Memory Layout (Per Connection)

A single contiguous allocation is registered as one Memory Region:

```
 ┌──────────────────────┬──────────────────────┬──────┬──────┐
 │   Send Ring Buffer   │   Recv Ring Buffer   │ Ofs  │ ROfs │
 │   (configurable)     │   (configurable)     │ (4B) │ (4B) │
 └──────────────────────┴──────────────────────┴──────┴──────┘
 ◄─── SendRingBufferSize ──►◄─ RecvRingBufferSize ─►

 Defaults: 64 KB each (MIN=4KB, MAX=4GB)
 Offset buffers only allocated when ring > 64KB
```

- **Send Ring Buffer**: Local staging area. Sender copies QUIC packet here, then RDMA Writes from it.
- **Recv Ring Buffer**: Landing zone. Remote peer writes QUIC packets directly into it via RDMA Write.
- **Offset Buffer** (optional): 4-byte head pointer exposed to remote peer via RDMA Read for flow control when ring > 64KB.

### Ring Buffer Protocol

Both send and recv rings are circular buffers with `Head` (consumer) and `Tail` (producer) pointers:

```
  Reserve(length):
    1. Check available = Capacity - CurSize
    2. If insufficient contiguous space at tail:
       - If Head < Tail: wrap Tail to 0 (insert padding entry in CompletionTable)
       - Recalculate available space after wrap
    3. Return Buffer[Tail], advance Tail += length

  Release(offset, length):
    1. If offset == Head (in-order): advance Head += length
       - Chase CompletionTable entries at new Head (handle wrap-arounds)
    2. If offset != Head (out-of-order): insert into CompletionTable hash
       - Will be chased when earlier completions arrive
```

**Out-of-order completion handling**: Since multiple RDMA Writes can be in-flight simultaneously,
completions may arrive out of order. A hash table (`SendCompletionTable` / `RecvCompletionTable`)
tracks pending releases. When an in-order completion arrives at `Head`, it chases forward through
the hash table releasing any contiguous completed entries.

### Send Data Path (CxPlatRdmaSend → RDMA Write)

```
RdmaSocketSend(Socket, Route, SendData):
  1. If SendQueue not empty → enqueue SendData → return BUFFER_TOO_SMALL
     (backpressure: wait for prior sends to complete)

  2. RdmaSocketSendInline(SocketProc, SendData):
     a. RdmaRemoteRecvRingBufferReserve(length)
        → get remote VA + offset where we can write
        → if no space: enqueue to SendQueue

     b. Encode ImmediateData:
        - If ring ≤ 64KB: imm = (offset << 16) | length  (16-bit each)
        - If ring > 64KB:  imm = length only (offset via RDMA Read)

     c. Build ND2_SGE pointing to SendRingBuffer[offset]
        - Buffer = SendData->Buffer.Buffer (already in send ring)
        - MemoryRegionToken = local MR token

     d. QueuePair->WriteWithImmediate(
           SGE, remote_addr, remote_rkey, flags, ImmediateData)

     e. SendCompletionQueue->Notify(ND_CQ_NOTIFY_ANY, overlapped)
        → IOCP completion fires → CxPlatIoRdmaSendEventComplete

  3. On SendCQ completion:
     a. GetManaResults() to drain CQ
     b. RdmaSendRingBufferRelease(offset, length) → free send ring slot
     c. RdmaRemoteReceiveRingBufferRelease(length) → update remote tracking
     d. Process pending SendQueue entries (RdmaSocketPendingSend)
```

### Receive Data Path (RDMA Write Arrival → QUIC Upcall)

```
CxPlatDataPathRdmaStartReceiveAsync(SocketProc):
  1. Pre-post receives (NdspiPostReceive) for doorbell notifications
  2. RecvCompletionQueue->Notify(ND_CQ_NOTIFY_ANY, overlapped)

On RecvCQ completion (CxPlatIoRdmaRecvEventComplete):
  1. GetManaResults() → array of ND2_MANA_RESULT
  2. For each result with RequestType == RecvRdmaWithImmediate:
     a. Decode ImmediateData:
        - Small ring: offset = (imm >> 16), length = (imm & 0xFFFF)
        - Large ring: length = imm, offset from RDMA Read of peer's offset buffer
     b. Build CXPLAT_RECV_DATA:
        - Buffer = RecvRingBuffer->Buffer[offset]
        - Length = decoded length
        - RingBufferOffset = offset  (for later release)
     c. Upcall: Datapath->RdmaHandlers.Receive(Socket, Context, RecvData)
        → QUIC core processes the packet (decrypt, parse frames, etc.)
     d. After QUIC is done with buffer:
        - RdmaLocalReceiveRingBufferRelease(offset, length)
        - Re-post receive for next doorbell
```

### Token Exchange (Post-Connect Handshake)

After RDMA connection is established but before data transfer, both peers must
exchange ring buffer metadata so each side knows the remote VA + rkey to write to:

```
Client (initiator):                       Server (acceptor):
  Connect completes                         Accept completes
  → State = Connected                       → State = Connected
  │                                         │
  BindMemoryWindow(RecvRing)                BindMemoryWindow(RecvRing)
  BindMemoryWindow(OffsetBuf)               BindMemoryWindow(OffsetBuf)
  │                                         │
  Post Recv (to receive server tokens)      Post Recv (to receive client tokens)
  │                                         │
  Build RDMA_DATAPATH_PRIVATE_DATA:         Wait for RecvCQ completion...
    { VA, capacity, rkey, offset_VA, rkey }  │
  Send(private_data) → ──────────────────── → RecvCQ fires
  │                                           Parse private_data
  │                                           Now knows client's recv ring
  │                                           │
  │                      ◄──────────────────── Send(private_data)
  RecvCQ fires                                │
  Parse private_data                          │
  Now knows server's recv ring                │
  │                                           │
  State = Ready                               State = Ready
```

When Memory Windows are NOT used (simpler path), the token exchange is embedded
in the connection's private data during Connect/Accept itself, eliminating the
post-connect Send/Recv phase.

### Immediate Data Encoding

Two modes depending on ring buffer size:

**Small rings (≤ 64KB)** — offset + length packed into 32-bit immediate:
```
  Bits [31:16] = offset in recv ring buffer (max 64KB)
  Bits [15:0]  = payload length (max 64KB)
```

**Large rings (> 64KB)** — only length in immediate, offset via RDMA Read:
```
  Bits [31:0] = payload length (max 16MB, capped by MAX_PAYLOAD_SIZE)
  Receiver reads sender's offset buffer via RDMA Read to determine position
```

### Completion Queue Processing

Uses **Windows IOCP (I/O Completion Ports)** for async completion notification:

```
  1. Post RDMA operation (Write, Send, Recv, etc.)
  2. CQ->Notify(ND_CQ_NOTIFY_ANY, &overlapped) → arms the CQ
  3. On completion: IOCP fires → event handler callback
  4. GetManaResults() → drain all completions (batch)
  5. Process each ND2_MANA_RESULT:
     - Check RequestType (Send, Recv, RecvRdmaWithImmediate, etc.)
     - Extract ImmediateData for RecvRdmaWithImmediate
     - Route to appropriate handler
```

Each connection has **12 distinct IOCP event handlers** for different async operations:
- `CxPlatIoRdmaRecvEventComplete` — data arrival
- `CxPlatIoRdmaSendEventComplete` — send completion
- `CxPlatIoRdmaConnectEventComplete` — connect completed
- `CxPlatIoRdmaConnectCompletionEventComplete` — CompleteConnect done
- `CxPlatIoRdmaGetConnectionRequestEventComplete` — accept new connection
- `CxPlatIoRdmaAcceptEventComplete` — accept completed
- `CxPlatIoRdmaDisconnectEventComplete` — peer disconnected
- `CxPlatIoRdmaTokenExchangeInitEventComplete` — token recv done
- `CxPlatIoRdmaTokenExchangeFinalEventComplete` — token send done
- `CxPlatIoRdmaSendRingBufferOffsetsEventComplete` — offset sync send
- `CxPlatIoRdmaRecvRingBufferOffsetsEventComplete` — offset sync recv
- `CxPlatIoRdmaReadRingBufferOffsetsEventComplete` — RDMA Read of offset

### Key Constants

| Constant | Value | Purpose |
|----------|-------|---------|
| `DEFAULT_RING_BUFFER_SIZE` | 64 KB | Default per-ring buffer size |
| `MAX_IMMEDIATE_RING_BUFFER_SIZE` | 64 KB | Threshold for offset buffer mode |
| `MIN_RING_BUFFER_SIZE` | 4 KB | Minimum allowed ring size |
| `MAX_RING_BUFFER_SIZE` | 4 GB | Maximum allowed ring size |
| `MIN_FREE_BUFFER_THRESHOLD` | 128 B | Minimum free space before backpressure |
| `MAX_PAYLOAD_SIZE` | 16 MB | Maximum single write payload |
| `DEFAULT_OFFSET_BUFFER_SIZE` | 4 B | Size of offset sync buffer |
| `MAX_SGE_POOL_SIZE` | 8192 | SGE pool capacity per connection |
| `MAX_MANA_RESULT_POOL_SIZE` | 8192 | CQ result pool capacity |
| `MAX_RDMA_CONNECTION_POOL_SIZE` | 1024 | Connection pool per adapter |
| `DEFAULT_RDMA_REQ_PRIVATE_DATA_SIZE` | 56 B | Client connect private data |
| `DEFAULT_RDMA_REP_PRIVATE_DATA_SIZE` | 196 B | Server accept private data |

### Backpressure / Flow Control

Flow control is **implicit** through ring buffer availability:

1. **Sender checks remote ring space**: Before each send, `RdmaRemoteRecvRingBufferReserve()`
   checks if the remote peer's recv ring has room. If not, the send is **queued** in `SendQueue`.

2. **Send queue drain**: When a send CQ completion fires, `RdmaSocketPendingSend()` drains
   the queue, attempting each pending send. If the remote ring is still full, the entry
   is re-inserted at the head (maintaining order).

3. **No explicit credits**: Unlike rsocket (which sends credit update messages), msquic relies
   on the ring buffer head/tail tracking + optional RDMA Read of the offset buffer for
   large rings.

### Comparison: Send/Recv (Our Design) vs Write-Into-Ring-Buffer (msquic)

| Aspect | Our Send/Recv | msquic RDMA Write + Ring |
|--------|--------------|-------------------------|
| **Copies** | 2 (sender copies to send MR, receiver reads from recv MR) | 1 on sender (to send ring), 0 on receiver (data lands in place) |
| **CPU on receiver** | Must post recv buffers; HCA writes to posted buffer | No recv posting for data; doorbell recv is tiny |
| **Protocol state** | None (implicit via RNR retry) | Ring head/tail, completion hash tables, offset sync |
| **Max in-flight** | Limited by recv buffer count (8) | Limited by ring buffer capacity (64KB default) |
| **Ordering** | Guaranteed by SQ FIFO | Guaranteed by SQ FIFO; ring wrap adds complexity |
| **Memory exposure** | None (two-sided) | Remote peer can write to recv ring via MW/MR |
| **Setup complexity** | 0 extra states | 7+ states for token exchange |
| **Code** | ~600 lines | ~6000 lines |

## References

- [microsoft/msquic#5113](https://github.com/microsoft/msquic/pull/5113) — RDMA NDSPI datapath PR
- [NetworkDirect SPI](https://learn.microsoft.com/en-us/windows-hardware/drivers/network/overview-of-network-direct-kernel-provider-interface--ndkpi-) — Windows RDMA abstraction
- [MANA (Microsoft Azure Network Adapter)](https://learn.microsoft.com/en-us/azure/virtual-network/accelerated-networking-mana-overview) — Azure SmartNIC
