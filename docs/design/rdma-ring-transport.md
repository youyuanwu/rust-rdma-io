# RDMA Ring Buffer Transport — Future Design

**Status:** Design (not implemented)  
**Date:** 2026-03-14  
**Prerequisite:** Transport trait (implemented in `rdma-io/src/transport.rs`)  
**Reference:** [rdma-transport-layer.md](rdma-transport-layer.md) — Transport architecture  
**Reference:** [rdma-transport-comparison.md](rdma-transport-comparison.md) — Three-way comparison

## Overview

`RdmaRingTransport` would implement the `Transport` trait using RDMA Write + Immediate Data
instead of Send/Recv — the same approach as msquic and rsocket. Since consumers are already
generic over `T: Transport`, this is a **drop-in replacement**:

```rust
let transport = RdmaRingTransport::connect(addr, RingConfig::default()).await?;
let stream = AsyncRdmaStream::new(transport);  // same AsyncRdmaStream<T>
```

## How It Differs from Send/Recv

| Operation | `RdmaTransport` (Send/Recv) | `RdmaRingTransport` (Write+Ring) |
|-----------|---------------------------|----------------------------------|
| `send_copy()` | Copy → MR[round_robin], `ibv_post_send(SEND)` | Check credit → copy → send_ring[tail], `ibv_post_send(WRITE_WITH_IMM)` |
| `poll_recv()` | Poll RecvCQ → `RecvCompletion` | Poll RecvCQ for doorbells + drain stash, decode immediate → virtual `RecvCompletion` |
| `recv_buf(idx)` | `recv_mrs[idx]` slice | `recv_ring[offset..offset+len]` via virtual_idx_map |
| `repost_recv(idx)` | `ibv_post_recv` on buffer | Advance ring Head, repost doorbell, send credit update |
| `send_copy` backpressure | Hardware RNR (always accepts) | Credit-based (`Ok(0)` when full) |
| Setup | Create QP, alloc MRs | Create QP, alloc rings, exchange tokens (VA + rkey + capacity) |

## Struct Design

```rust
pub struct RdmaRingTransport {
    // CQ poll state and connection flags
    send_cq_state: CqPollState,
    recv_cq_state: CqPollState,
    disconnected: bool,
    qp_dead: bool,
    peer_disconnected: bool,

    // Virtual buffer index mapping: ring offsets → discrete indices
    virtual_idx_map: HashMap<usize, (usize, usize)>,  // virt_idx → (offset, len)
    next_virt_idx: usize,

    // Stashed completions: doorbells polled from CQ but not yet returned.
    // CRITICAL: losing doorbell immediate data = losing data location in ring.
    recv_stash: VecDeque<RecvCompletion>,

    qp: AsyncQp,

    // Ring buffers
    send_ring: RingBuffer,       // Local staging → RDMA Write source
    recv_ring: RingBuffer,       // Landing zone for remote writes
    remote_ring: RemoteRingInfo, // Peer's recv ring VA + rkey + capacity

    // Doorbell recv buffers (small — just for immediate data notification)
    doorbell_bufs: Vec<OwnedMemoryRegion>,

    // Credit tracking: sender's estimate of remote ring free space.
    // Without this, sender can overwrite unconsumed data (silent corruption).
    remote_credits: usize,

    // Completion tracking for out-of-order release
    send_completions: CompletionTracker,
    recv_completions: CompletionTracker,

    // CM resources — drop order: AsyncFd → CmId → EventChannel
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,
}

struct RingBuffer {
    mr: OwnedMemoryRegion,
    capacity: usize,
    head: usize,  // Consumer position
    tail: usize,  // Producer position
}

struct RemoteRingInfo {
    addr: u64,       // Remote virtual address
    rkey: u32,       // Remote memory key
    capacity: usize,
    head: usize,     // Tracked via credits
    tail: usize,
}
```

## The buf_idx Mapping Trick

The `Transport` trait uses `buf_idx` for `recv_buf()` and `repost_recv()`. The ring has no
discrete buffers — it has a contiguous byte range. Solution: **virtual buffer indices**.

`poll_recv` decodes each doorbell's immediate data → `(ring_offset, length)`, assigns a
monotonically increasing `virt_idx`, stores the mapping in `virtual_idx_map`, and returns
`RecvCompletion { buf_idx: virt_idx, byte_len: length }`. Consumers use `virt_idx`
identically to discrete buffer indices.

## Send Path

```
send_copy(data):
  1. Check remote_credits > 0 → if 0, return Ok(0)
  2. Alloc local send_ring slot at tail
  3. Copy data → send_ring[tail]
  4. Alloc remote ring slot, decrement remote_credits
  5. Encode immediate: (remote_offset << 16) | length
  6. ibv_post_send(WRITE_WITH_IMM, local → remote, imm)
  7. Return Ok(len)
```

## Recv Path

```
poll_recv:
  1. Drain recv_stash first (overflow from previous poll)
  2. Poll RecvCQ for Write-With-Immediate doorbells
  3. Decode immediate → offset + length
  4. Data already in recv_ring[offset] — zero-copy on receiver!
  5. Allocate virtual buf_idx, store in virtual_idx_map
  6. If output buffer full, STASH remainder (can't lose doorbells!)
  7. Return completions

repost_recv(idx):
  1. Remove virtual_idx_map entry → (offset, length)
  2. Advance recv_ring head (with out-of-order tracking)
  3. Repost doorbell recv buffer
  4. Send credit update to peer
```

## Token Exchange (Connection Setup)

```
  Client                                Server
    │ Connect(QP)                         │ Accept(QP)
    │ Bind MW to recv_ring                │ Bind MW to recv_ring
    │ Post doorbell recv                  │ Post doorbell recv
    │                                     │
    │ Send(my_ring_VA + rkey + capacity)  │
    │ ─────────────────────────────────►  │
    │                                     │ Parse → store as remote_ring
    │  ◄─────────────────────────────────  │
    │ Parse → store as remote_ring        │ Send(my_ring_VA + rkey + capacity)
    │                                     │
    │ Ready ◄─────────────────────────────► Ready
```

## Behavioral Differences for Consumers

| Behavior | Send/Recv | Ring |
|----------|-----------|------|
| `send_copy` backpressure | Always accepts (hardware RNR) | May return `Ok(0)` when ring full |
| `recv_pending` (partial read) | Blocks 1 of N independent buffers | Pins ring Head → **app-level HOL blocking** |
| Credit management | None needed (hardware) | Must send credit updates on `repost_recv` |

**Datagram** (copy-and-repost immediately): unaffected by ring HOL.  
**Stream** (partial reads hold buffer): degrades — prefer Send/Recv for streams.

## Trade-offs

```
                     RdmaTransport              RdmaRingTransport
                     (Send/Recv)                (Write+Ring)
                ┌────────────────────┐    ┌────────────────────────┐
  Simplicity    │ ████████████████   │    │ ███                    │
  Portability   │ ████████████████   │    │ ██████████             │
  Recv copies   │ ████ (2 copies)    │    │ ████████████████ (1)   │
  Throughput    │ ████████████       │    │ ████████████████       │
  Latency       │ ██████████████     │    │ ████████████████       │
  Code size     │ ████████████████   │    │ ██████                 │
  Safety        │ ████████████████   │    │ ████████ (MW needed)   │
  Stream HOL    │ ████████████████   │    │ ████ (ring Head blocks)│
                └────────────────────┘    └────────────────────────┘

  Use RdmaTransport:     QUIC, RPC, byte streams, many peers
  Use RdmaRingTransport: Datagram bulk transfer, max throughput, few peers
```

## Open Issues (deferred)

- **Credit protocol:** Piggyback on next data send vs dedicated 0-byte message. Batching strategy.
- **64KB limit:** Immediate data `(offset<<16|length)` limits ring to 64KB. Need large-ring mode (length-only encoding + RDMA Read for offset) for stream workloads.
