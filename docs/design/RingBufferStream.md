# Ring Buffer Design for AsyncRdmaStream

**Status:** Design / Research  
**Date:** 2026-03-07

## Motivation

AsyncRdmaStream currently uses two-sided **Send/Recv** for all data transfer. Every message incurs two memory copies (sender → send_mr, recv_mr → caller) and requires 8 pre-posted receive buffers per connection. An RDMA Write-based ring buffer design eliminates the receiver-side copy and decouples sender throughput from receiver polling speed.

This doc explores how to retrofit AsyncRdmaStream with a ring buffer data path, informed by msquic-RDMA and rsocket architectures.

## Current Architecture (Send/Recv)

```
Writer                               Reader
  app buf → memcpy → send_mr           recv_mrs[i] → memcpy → app buf
  post_send(send_mr)                   [completion arrives on recv_cq]
  poll_send_cq()                       re-post recv_mrs[i]
```

**Per-connection memory:** 1×64KB send + 8×64KB recv = 576 KB  
**Copies per message:** 2  
**Completions per message:** 2 (send CQ + recv CQ)

## Proposed Ring Buffer Architecture

### Overview

Replace pre-posted recv buffers with a single large **recv ring buffer**. The sender writes directly into the remote peer's ring buffer via RDMA Write, then sends a small doorbell message (Send with Immediate Data) to notify the receiver.

```
Writer (poll_write)                     Reader (poll_read)
  app buf → memcpy → send_ring[tail]      [doorbell arrives on recv_cq]
  RDMA Write → remote recv_ring[tail]     read from recv_ring[offset]
  Send w/ Immediate (offset, len)         advance head, return data
  poll_send_cq()                          post_recv() for next doorbell
```

### Memory Layout

Each side allocates two registered buffers:

```
┌─────────────────────────────────────────┐
│ Send Ring Buffer (send_mr)              │  e.g. 256 KB
│  [head ──── data ──── tail ──→ free]    │  RDMA Write source
└─────────────────────────────────────────┘

┌─────────────────────────────────────────┐
│ Recv Ring Buffer (recv_mr)              │  e.g. 256 KB
│  [head ──── data ──── tail ──→ free]    │  Remote peer writes here
│  Remote access: rkey exchanged at setup │
└─────────────────────────────────────────┘

┌───────────────────┐
│ Doorbell Buffers   │  Small pre-posted recv buffers for
│ (doorbell_mrs[N]) │  Send-with-Immediate notifications
└───────────────────┘
```

### Struct Changes

```rust
const RING_BUF_SIZE: usize = 256 * 1024;  // 256 KB default
const NUM_DOORBELL_BUFS: usize = 8;       // doorbell recv buffers

pub struct AsyncRdmaStream {
    // Ring buffers
    send_ring: RingBuffer,              // local send staging
    recv_ring: RingBuffer,              // remote peer writes here
    recv_mr: OwnedMemoryRegion,         // registered recv ring (LOCAL_WRITE + REMOTE_WRITE)
    send_mr: OwnedMemoryRegion,         // registered send ring (LOCAL_WRITE)

    // Remote peer's recv ring info (exchanged at connection time)
    remote_addr: u64,                   // virtual address of peer's recv ring
    remote_rkey: u32,                   // rkey for RDMA Write into peer's recv ring
    remote_capacity: u32,               // peer's ring buffer size
    remote_tail: u32,                   // our write position in peer's ring

    // Doorbell buffers (small, for Send-with-Imm notifications)
    doorbell_mrs: [OwnedMemoryRegion; NUM_DOORBELL_BUFS],
    
    // Credit tracking
    remote_credits: u32,                // how much space peer has available
    local_credits_acked: u32,           // credits we've sent back to peer

    // Existing fields
    qp: AsyncQp,
    send_cq_state: CqPollState,
    recv_cq_state: CqPollState,
    // ...
}

struct RingBuffer {
    head: u32,       // consumer position (read from here)
    tail: u32,       // producer position (write to here)
    capacity: u32,
}
```

### Connection Setup: Token Exchange

After the RDMA CM connection is established, both peers must exchange memory region tokens so each can RDMA Write into the other's recv ring. This is a new handshake phase that does not exist in the current Send/Recv design.

```
Client                                  Server
  ├─ connect()                            ├─ accept()
  │                                       │
  │  ── Send(my_addr, my_rkey, cap) ──→   │
  │  ←── Send(my_addr, my_rkey, cap) ──   │
  │                                       │
  ├─ store remote_addr, remote_rkey       ├─ store remote_addr, remote_rkey
  ├─ READY                                ├─ READY
```

Token exchange message (packed struct, ~20 bytes):
```rust
#[repr(C, packed)]
struct TokenExchange {
    recv_ring_addr: u64,    // virtual address of recv_mr
    recv_ring_rkey: u32,    // rkey for remote write access
    recv_ring_capacity: u32,
}
```

The recv MR must be registered with **`REMOTE_WRITE`** access in addition to `LOCAL_WRITE`:
```rust
recv_mr = pd.reg_mr_owned(
    vec![0u8; RING_BUF_SIZE],
    AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE,
)?;
```

### Data Path: poll_write

```rust
fn poll_write(cx, buf) -> Poll<Result<usize>> {
    // 1. Check remote credits (flow control)
    let send_len = min(buf.len(), remote_credits, send_ring.free_space());
    if send_len == 0 {
        // No space — need to wait for credit update from peer
        return Poll::Pending;
    }

    // 2. Copy data into local send ring (handles wrap-around)
    send_ring.write(buf[..send_len], &mut send_mr);

    // 3. RDMA Write into remote recv ring
    qp.post_write_with_imm(
        &send_mr, send_ring.tail - send_len, send_len,  // local source
        remote_addr + remote_tail, remote_rkey,           // remote dest
        encode_imm(remote_tail, send_len),                // immediate: offset + length
        SEND_WR_ID,
    )?;

    // 4. Advance remote tail, deduct credits
    remote_tail = (remote_tail + send_len) % remote_capacity;
    remote_credits -= send_len;

    // 5. Poll send CQ for completion
    poll_send_cq(cx) ...
}
```

### Data Path: poll_read

```rust
fn poll_read(cx, buf) -> Poll<Result<usize>> {
    // 1. Check if data available in recv ring
    if recv_ring.has_data() {
        return copy_from_ring_and_ack(buf);
    }

    // 2. Poll recv CQ for doorbell (Write-with-Immediate completion)
    match qp.poll_recv_cq(cx, &mut recv_cq_state, &mut wc_buf) {
        Poll::Ready(Ok(n)) => {
            let imm = wc_buf[0].imm_data();
            let (offset, len) = decode_imm(imm);

            // 3. Data is already in recv_mr at [offset..offset+len]
            //    (arrived via RDMA Write — no copy needed on receive side!)
            recv_ring.advance_tail(len);

            // 4. Copy from recv ring to caller's buffer
            let copied = recv_ring.read(buf, &recv_mr);
            
            // 5. Re-post doorbell recv buffer
            qp.post_recv_buffer(&doorbell_mrs[i], i as u64)?;

            // 6. Send credit update to peer (piggyback or dedicated)
            maybe_send_credits();

            Poll::Ready(Ok(copied))
        }
        Poll::Pending => Poll::Pending,
    }
}
```

### Flow Control: Credits

The receiver must tell the sender how much ring space is available. Without this, the sender could overwrite unconsumed data.

**Options:**

1. **Piggyback on data:** Embed local credits in the immediate data of outgoing Write-with-Imm messages (works for bidirectional traffic like HTTP/2)

2. **Dedicated credit Send:** When the receiver consumes data, Send a small credit update message to the peer. Requires extra doorbell recv buffers on the peer.

3. **RDMA Read of remote offset:** The sender periodically reads the remote peer's head pointer via RDMA Read to compute available space (msquic approach)

For simplicity, option 2 is recommended — the credit Send is small (4 bytes) and naturally fits the existing CQ notification path.

### Wrap-Around Handling

Ring buffers wrap when tail reaches capacity. Two strategies:

**A. Split writes (memcpy-friendly):**
```
If data wraps around end of ring:
  Write chunk1 to [tail..capacity]  → RDMA Write #1
  Write chunk2 to [0..remainder]    → RDMA Write #2
  Send doorbell with both offsets
```

**B. Guard region (simpler):** If data doesn't fit contiguously, skip to offset 0 and waste the tail gap. Simpler but wastes up to `max_msg_size` bytes per wrap.

msquic uses strategy A with a completion hashtable for out-of-order tracking. For our use case (gRPC messages typically <64KB, ring ≥256KB), strategy B is simpler and wastes <25% in the worst case.

## Transport Compatibility

### rxe (Soft-RoCE) — Full Support

rxe emulates InfiniBand (RoCE v2) and supports all operations required by this design:

| Capability | rxe | Required by ring buffer? |
|-----------|-----|------------------------|
| RDMA Write | ✅ | Yes — bulk data transfer |
| Write with Immediate | ✅ (mandatory per IB spec) | Yes — doorbell notification |
| RDMA Read | ✅ | Optional — offset sync (msquic approach) |
| Memory Windows (Type 2A) | ✅ | Optional — security hardening |
| Atomics (CAS/FAA) | ✅ (`ATOMIC_HCA`) | No |

rxe device flags: `0x01223c76` — includes `MEM_WINDOW_TYPE_2A`, `RC_RNR_NAK_GEN`, `SRQ_RESIZE`.

**Note:** rsocket failed on rxe, but that was a bug in rsocket's protocol layer, not in RDMA verbs. Our design uses raw ibverbs directly — Write, Write+Imm, Send, and Recv all work individually on rxe (verified via rping, ucmatose, and our `async_qp_rdma_write_read` test).

### siw (iWARP) — Requires Fallback

**Write with Immediate Data** is required for the doorbell mechanism. This is an InfiniBand/RoCE operation — standard iWARP does NOT support it.

**Options for iWARP:**

1. **Separate doorbell Send:** Use plain `Send` (no immediate data) with the offset/length encoded in the message payload. Costs an extra recv buffer per notification but works on all transports.

2. **Dual-mode:** Use Write+Imm on IB/RoCE, fall back to Write + separate Send on iWARP. Adds code complexity but preserves optimal path per transport.

3. **Drop iWARP support:** Only use ring buffers on IB/RoCE. Keep Send/Recv for iWARP connections.

Option 1 is the most portable — it replaces Write-with-Immediate with a Write followed by a Send containing the metadata. The Send naturally generates a recv completion, serving as the doorbell. Cost: one extra small message per data transfer.

```
IB/RoCE:   Write-with-Imm(data, offset|len)     → 1 operation
iWARP:     Write(data) + Send(offset|len)        → 2 operations
```

## Complexity Analysis

| Component | Send/Recv (current) | Ring Buffer (proposed) |
|-----------|--------------------|-----------------------|
| **Connection setup** | Connect/Accept only | + token exchange handshake |
| **MR registration** | LOCAL_WRITE only | + REMOTE_WRITE (security exposure) |
| **Send path** | post_send + poll | Write + doorbell + poll + credit check |
| **Recv path** | poll + copy + re-post | poll doorbell + read ring + send credits |
| **Flow control** | Implicit (buffer count) | Explicit credit tracking |
| **Disconnect** | DREQ/DREP | + invalidate remote access |
| **Buffer management** | Fixed alloc/free | Ring head/tail + wrap-around |
| **Lines of code** | ~600 | ~1200-1500 (estimated) |

## When to Use Ring Buffers

**Worthwhile when:**
- Large message throughput matters (>16KB per message)
- Running on InfiniBand or RoCE hardware (not software emulation)
- Connection count is moderate (ring buffers use more memory per-conn)
- Latency of receiver-side copy is measurable in profiles

**Not worthwhile when:**
- Messages are small (<4KB, typical gRPC/RPC) — copy cost is negligible
- iWARP/siw support is required — degrades to Write + Send (loses most benefit)
- Connection count is high — 512KB ring per direction vs 576KB fixed buffers is similar
- Simplicity and portability are priorities

## Recommendation

Implement ring buffers as an **opt-in mode** behind a feature flag or builder option, not as a replacement for Send/Recv:

```rust
let stream = AsyncRdmaStream::connect_with_ring_buffer(addr, RingBufferConfig {
    send_ring_size: 256 * 1024,
    recv_ring_size: 256 * 1024,
    doorbell_count: 4,
})?;
```

The default path remains Send/Recv for maximum portability and simplicity. Ring buffers are available for InfiniBand/RoCE deployments where large-message throughput justifies the added complexity.

## References

- [msquic RDMA transport](msquic-rdma.md) — Microsoft's ring buffer implementation over NDSPI (~6000 lines)
- [rsocket analysis](../design/Rsocket.md) — librdmacm ring buffer implementation, siw/rxe compatibility testing
- [RDMA operations analysis](../design/RdmaOperations.md) — Send/Recv vs Write trade-offs, academic references
