# rsocket: RDMA Byte Stream via librdmacm

**Status:** Research  
**Date:** 2026-03-07  
**Validated:** 2026-03-14 (rdma-core 50.0, kernel 6.17.0-1008-azure)

## Overview

rsocket is a POSIX-compatible socket API built on RDMA, shipped as part of `librdmacm` (rdma-core). It provides two API families:

- **`rsend()`/`rrecv()`** — byte stream using RDMA Write with Immediate Data (tested via `rstream`)
- **`riowrite()`/`rioread()`** — remote I/O using RDMA Read/Write without immediate data (tested via `riostream`)

## Architecture

```
rsend/rrecv (rstream):                riowrite/rioread (riostream):
  Sender rsend(buf)                     Writer riowrite(buf)
    ├─ RDMA Write → remote ring buf       ├─ RDMA Write → remote buffer
    ├─ Send Immediate (doorbell)          └─ (no doorbell needed)
    └─ advance local tail pointer
  Receiver rrecv(buf)                   Reader rioread(buf)
    ├─ poll for new data                  ├─ RDMA Read ← remote buffer
    ├─ read from ring position            └─ (direct read, no notification)
    └─ advance head, send credit
```

**Ring buffers:** Both sides maintain pre-registered ring buffers. The sender writes directly into the receiver's buffer via one-sided RDMA Write — the receiver's CPU is not involved in the data transfer itself.

**Credit-based flow control:** The receiver advertises available credits (free ring slots). The sender blocks when credits are exhausted. Credits are replenished via small Send messages from receiver to sender.

**Immediate data as doorbell:** RDMA Write with Immediate Data generates a receive completion on the remote side, notifying it that new data has arrived without a separate signaling message. This is used by `rsend`/`rrecv` but not by `riowrite`/`rioread`.

## Availability

rsocket is part of `librdmacm` (rdma-core). Installed on this machine:

| Component | Details |
|-----------|---------|
| Header | `/usr/include/rdma/rsocket.h` (via `librdmacm-dev`) |
| Library | `librdmacm1t64` v50.0 |
| Tools | `rdmacm-utils`: `rstream`, `riostream`, `rping`, `ucmatose` |
| Rust integration | FFI via `-lrdmacm`, or bindgen on `rsocket.h` |

### Tokio Integration Challenge

rsocket is a synchronous C library that manages its own event loop and internal buffers. Integrating it with tokio's async model would require:

- Bridging rsocket's blocking `rpoll()` with tokio's `AsyncFd`
- Managing rsocket's internal buffer lifecycle alongside Rust ownership
- Significant unsafe FFI surface for a marginal performance gain

This makes it impractical as a drop-in replacement for our `AsyncRdmaStream`.

## Transport Compatibility Testing

All tests: loopback on `eth0` (10.0.0.4), rdma-core 50.0, kernel 6.17.0-1008-azure.

### siw (iWARP)

| Tool | API | Status | Notes |
|------|-----|--------|-------|
| `rstream` | `rsend`/`rrecv` | ❌ | "Connection reset by peer" — all sizes, all modes (`-T b`, `-T r`) |
| `riostream` | `riowrite`/`rioread` | ⚠️ | 64B works (4.66 µs/xfer); ≥4KB hangs (timeout after 15s) |
| `rstream -T s` | TCP sockets | ✅ | 64B: 4.14 µs/xfer (confirms tool itself works) |
| `rping` | rdma_cm verbs | ✅ | 5/5 pings |

### rxe (Soft-RoCE, out-of-tree build)

| Tool | API | Status | Notes |
|------|-----|--------|-------|
| `rstream` | `rsend`/`rrecv` | ❌ | "Connection reset by peer" — all sizes |
| `riostream` | `riowrite`/`rioread` | ⚠️ | 64B works (2.97 µs/xfer); ≥4KB hangs (timeout after 15s) |
| `rping` | rdma_cm verbs | ✅ | 5/5 pings |
| `ucmatose` | rdma_cm Send/Recv | ✅ | Full data transfer |
| `ibv_rc_pingpong -g 0` | raw RC verbs | ✅ | ~1240 Mbit/s, 52 µs/iter |

### Summary

| Tool / API | siw | rxe | Mechanism |
|------------|-----|-----|-----------|
| `rstream` (rsend/rrecv) | ❌ | ❌ | RDMA Write + Immediate Data |
| `riostream` (riowrite/rioread) | ⚠️ 64B only | ⚠️ 64B only | RDMA Read/Write |
| `rping` (rdma_cm) | ✅ | ✅ | RDMA Read/Write via CM |
| `ucmatose` (rdma_cm) | ✅ | ✅ | Send/Recv via CM |

**Key finding:** `rsend`/`rrecv` (the byte-stream API) is completely broken on both software providers. `riowrite`/`rioread` works only for small transfers. Only the underlying rdma_cm verbs (`rping`, `ucmatose`) are reliable.

## Trade-offs vs Send/Recv

| Aspect | Send/Recv (our approach) | rsocket (Write-based) |
|--------|-------------------------|----------------------|
| **Copies per message** | 2 (sender + receiver) | 1 (sender only) |
| **Receiver CPU** | Active (post + process) | Passive (data arrives in-place) |
| **Setup complexity** | Low | High (exchange MR keys, manage ring) |
| **Flow control** | Implicit (buffer count) | Explicit credits (more protocol code) |
| **Software provider support** | ✅ Full (siw + rxe) | ❌ Broken on both |
| **Memory per conn** | 576KB (1 send + 8 recv) | Similar (ring buffers) |
| **Small msg latency** | ~same | Slightly lower (in theory) |
| **Large msg throughput** | Lower (extra copy) | Higher (zero-copy recv, in theory) |

## Why We Don't Use rsocket

1. **Broken on software providers:** `rsend`/`rrecv` fails on both siw and rxe (rdma-core 50.0). `riowrite`/`rioread` only works for small (64B) transfers. We need both providers for development and CI
2. **iWARP limitation:** RDMA Write with Immediate Data (used by rsend/rrecv) is an InfiniBand/RoCE feature. Pure iWARP devices may not support it, limiting portability
3. **Tokio integration cost:** rsocket manages its own event loop. Bridging with async Rust would require substantial unsafe FFI with marginal benefit
4. **Marginal gain for RPC:** Our primary workload is gRPC/tonic with small messages (<1KB). The saved receiver-side copy is negligible at these sizes
5. **Complexity:** Credit-based flow control, ring pointer synchronization, and MR key exchange add significant protocol surface vs simple Send/Recv

---

## Internal Implementation Analysis

*Source: [rdma-core/librdmacm/rsocket.c](https://github.com/linux-rdma/rdma-core/blob/master/librdmacm/rsocket.c) (~3400 lines)*

This section documents rsocket's internal RDMA API usage and data flow logic for reference when designing our own RDMA byte stream.

### RDMA Verbs API Usage

rsocket uses both rdma_cm (connection management) and libibverbs (data path) APIs:

#### Connection Management (rdma_cm)
| Function | Where Used | Purpose |
|----------|-----------|---------|
| `rdma_create_id(NULL, &cm_id, rs, RDMA_PS_TCP)` | `rsocket()` | Create CM ID for stream socket |
| `rdma_bind_addr()` | `rbind()` | Bind to local address |
| `rdma_listen()` | `rlisten()` | Start listening for connections |
| `rdma_get_request()` | `rs_accept()` | Accept incoming connection |
| `rdma_resolve_addr()` | `rs_do_connect()` | Resolve destination address |
| `rdma_resolve_route()` | `rs_do_connect()` | Resolve RDMA route |
| `rdma_connect()` | `rs_do_connect()` | Initiate RDMA connection |
| `rdma_accept()` | `rs_accept()` | Accept with connection parameters |
| `rdma_create_qp()` | `rs_create_ep()` | Create RC queue pair |
| `rdma_destroy_qp()` | `rs_free()` | Destroy queue pair |
| `rdma_destroy_id()` | `rs_free()` | Destroy CM ID |

#### Memory Registration
| Function | Where Used | Purpose |
|----------|-----------|---------|
| `rdma_reg_msgs(cm_id, sbuf, size)` | `rs_init_bufs()` | Register send buffer (local read) |
| `rdma_reg_write(cm_id, rbuf, size)` | `rs_init_bufs()` | Register recv buffer (remote write access) |
| `rdma_reg_write(cm_id, target_sgl, len)` | `rs_init_bufs()` | Register target SGL (remote write access) |
| `rdma_dereg_mr()` | `rs_free()` | Deregister memory regions |
| `ibv_reg_mr(pd, buf, len, access)` | `riomap()` | Register iomap memory region |

#### Completion Queue
| Function | Where Used | Purpose |
|----------|-----------|---------|
| `ibv_create_comp_channel()` | `rs_create_cq()` | Create CQ notification channel |
| `ibv_create_cq(verbs, sq+rq_size, ...)` | `rs_create_cq()` | Create **single shared CQ** for send+recv |
| `ibv_req_notify_cq()` | `rs_process_cq()` | Arm CQ for next completion event |
| `ibv_get_cq_event()` | `rs_get_cq_event()` | Block waiting for CQ event (via comp_channel fd) |
| `ibv_ack_cq_events()` | `rs_get_cq_event()` | Acknowledge CQ events (batched) |
| `ibv_poll_cq()` | `rs_poll_cq()` | Poll for completions |

#### Data Transfer
| Function | Opcode | Where Used | Purpose |
|----------|--------|-----------|---------|
| `ibv_post_send()` | `IBV_WR_RDMA_WRITE_WITH_IMM` | `rs_post_write_msg()` | Data transfer + doorbell (InfiniBand/RoCE) |
| `ibv_post_send()` | `IBV_WR_RDMA_WRITE` | `rs_post_write()` | Pure RDMA Write (no notification) |
| `ibv_post_send()` | `IBV_WR_SEND` (inline) | `rs_post_msg()`, `rs_post_write_msg()` | iWARP fallback: inline Send as doorbell |
| `ibv_post_recv()` | — | `rs_post_recv()` | Post receive buffer (for Imm Data or inline msg) |

### QP Configuration

```c
qp_attr.qp_type          = IBV_QPT_RC;       // Reliable Connected
qp_attr.sq_sig_all       = 1;                // Signal all send completions
qp_attr.cap.max_send_wr  = sq_size;          // Default 384
qp_attr.cap.max_recv_wr  = rq_size;          // Default 384
qp_attr.cap.max_send_sge = 2;                // For wrap-around ring buffer
qp_attr.cap.max_recv_sge = 1;                // Single recv SGE
qp_attr.cap.max_inline_data = sq_inline;     // Default 64 bytes

// Connection parameters (rdma_connect):
param.flow_control    = 1;
param.retry_count     = 7;
param.rnr_retry_count = 7;                   // Infinite RNR retry
// iWARP: param.initiator_depth = 1 (work-around for RDMA read during connect)
```

**Key design choice:** Single CQ for both send and recv (`send_cq == recv_cq`). This simplifies processing but requires distinguishing send vs recv completions via `wr_id` flags.

### Buffer Architecture

Each connection allocates 4 registered memory regions:

```
┌─────────────────────────────────────────────────────────────────┐
│ Send Buffer (smr) — rdma_reg_msgs                               │
│ ┌────────────────────────┬──────────────────────────┐           │
│ │ sbuf[sbuf_size]        │ ctrl_msgs[CTRL_SIZE * 4] │           │
│ │ (ring buffer for data) │ (if inline < ctrl_msg)   │           │
│ └────────────────────────┴──────────────────────────┘           │
├─────────────────────────────────────────────────────────────────┤
│ Recv Buffer (rmr) — rdma_reg_write (REMOTE WRITABLE)            │
│ ┌────────────────────────┬──────────────────────────┐           │
│ │ rbuf[rbuf_size]        │ msg_buf[rq_size * 4]     │           │
│ │ (remote writes here)   │ (iWARP: inline msg recv) │           │
│ └────────────────────────┴──────────────────────────┘           │
├─────────────────────────────────────────────────────────────────┤
│ Target SGL (target_mr) — rdma_reg_write (REMOTE WRITABLE)       │
│ ┌──────────────────────────┬──────────────────────┐             │
│ │ target_sgl[2]            │ target_iomap[N]      │             │
│ │ (2 SGL entries for ring) │ (for riowrite/read)  │             │
│ └──────────────────────────┴──────────────────────┘             │
├─────────────────────────────────────────────────────────────────┤
│ Message Queue (malloc, not registered)                           │
│ rmsg[rq_size + 1] — circular buffer of {op, data} completions   │
└─────────────────────────────────────────────────────────────────┘

Defaults: sbuf_size = 128KB, rbuf_size = 128KB, sq/rq_size = 384
Total registered memory per connection: ~256KB + SGL + iomap
```

### Connection Handshake — Private Data Exchange

During `rdma_connect`/`rdma_accept`, both peers exchange `rs_conn_data` in the CM private data:

```c
struct rs_conn_data {
    uint8_t   version;             // Protocol version (must be 1)
    uint8_t   flags;               // RS_CONN_FLAG_NET | RS_CONN_FLAG_IOMAP
    uint16_t  credits;             // Initial recv credits (= rq_size)
    uint8_t   reserved[3];
    uint8_t   target_iomap_size;   // Number of iomap entries
    struct rs_sge target_sgl;      // Remote addr+rkey+length for SGL
    struct rs_sge data_buf;        // Remote addr+rkey+length for rbuf (half)
};
```

**What's exchanged:**
1. **target_sgl** — Address/rkey of the remote's `target_sgl[]` array (so the sender can update the receiver's SGL via RDMA Write)
2. **data_buf** — Address/rkey of the remote's `rbuf` (so the sender can write data into the receiver's ring buffer)
3. **credits** — Initial number of receive WRs posted (flow control starting point)

After this exchange, each side knows where to RDMA Write data into the peer's buffers.

### Immediate Data Format

The 32-bit immediate data encodes the operation and payload size:

```
Bit 31 (msg type):  0 = data,  1 = control
Bit 30 (buf type):  0 = target buffer,  1 = direct-receive
Bit 29 (more):      0 = end of transfer,  1 = more data

For data (bits 31:29 = 000):  bits[28:0] = bytes transferred
For SGL  (bits 31:29 = 100):  bits[28:0] = receive credits granted
For CTRL (bits 31:29 = 111):  bits[28:0] = control code (disconnect/shutdown/keepalive)
```

```c
#define rs_msg_set(op, data)  ((op << 29) | (uint32_t)(data))
#define rs_msg_op(imm_data)   (imm_data >> 29)
#define rs_msg_data(imm_data) (imm_data & 0x1FFFFFFF)
```

This means max single transfer = 512MB (2²⁹ - 1 bytes), but rsocket caps at `RS_MAX_TRANSFER = 65536`.

### rsend Data Path (RDMA Write + Immediate Data)

**Sender side (`rsend` → `rs_write_data`):**

```
rsend(socket, buf, len):
  1. Check rs_can_send() — need: sqe_avail, sbuf_bytes_avail, credits (sseq_no != sseq_comp),
     and target_sgl has space
  2. If can't send, call rs_get_comp() which:
     a. Busy-polls CQ for `polling_time` (default 10µs)
     b. If still not ready, arms CQ and blocks on ibv_get_cq_event()
  3. Clamp xfer_size to min(left, sbuf_bytes_avail, target_sgl.length, RS_MAX_TRANSFER)
  4. Start with 2KB transfer, double each iteration up to 64KB (overlapped sending)
  5. Copy user data into sbuf (ring buffer with wrap-around using 2 SGEs)
  6. Post RDMA Write with Immediate:
     - remote_addr = target_sgl[target_sge].addr  (receiver's rbuf position)
     - rkey        = target_sgl[target_sge].key
     - imm_data    = RS_OP_DATA | bytes_transferred
  7. Advance target_sgl pointer; if segment exhausted, move to next SGL entry
  8. Decrement sqe_avail, sbuf_bytes_avail; increment sseq_no
```

**Receiver side (`rrecv` → `rs_poll_cq`):**

```
rrecv(socket, buf, len):
  1. Check rs_have_rdata() — rmsg_head != rmsg_tail
  2. If no data, call rs_get_comp() (busy-poll then block)
  3. rs_poll_cq() processes completions:
     For each recv completion (IBV_WC_WITH_IMM):
       - Decode imm_data → op=RS_OP_DATA, data=byte_count
       - Store in rmsg[rmsg_tail] = {op, data}; advance tail
       - Re-post recv WR
     For each send completion:
       - Recover sqe_avail, sbuf_bytes_avail
       - Advance ctrl_max_seqno for control messages
  4. Copy data from rbuf[rbuf_offset] to user buffer
     (handles ring wrap-around: end_size check)
  5. Advance rbuf_offset, rbuf_bytes_avail
  6. Increment rseq_no (tracks received messages for credit flow)
  7. Call rs_update_credits() — may send credit update to sender
```

### iWARP Adaptation (RS_OPT_MSG_SEND)

iWARP does **not** support `IBV_WR_RDMA_WRITE_WITH_IMM`. rsocket detects iWARP transport and sets `RS_OPT_MSG_SEND`:

```c
if (cm_id->verbs->device->transport_type == IBV_TRANSPORT_IWARP)
    rs->opts |= RS_OPT_MSG_SEND;
```

**Impact on data path:**
- Instead of one `RDMA_WRITE_WITH_IMM`, sends **two** WRs per message:
  1. `IBV_WR_RDMA_WRITE` — data into remote ring buffer (no notification)
  2. `IBV_WR_SEND` (inline) — 4-byte message with op+length (acts as doorbell)
- Receiver must post recv buffers with actual SGEs pointing to msg_buf area
- Requires `sqe_avail >= 2` per send (double the WR consumption)
- `sq_inline` must be at least `RS_MSG_SIZE` (4 bytes)
- Each recv WR carries a 4-byte SGE: `rbuf + rbuf_size + (msg_index * 4)`

```c
// iWARP: rs_post_write_msg posts TWO work requests
ret = rs_post_write(rs, sgl, nsge, msg, flags, addr, rkey);   // RDMA Write
if (!ret) {
    // Then inline Send as doorbell
    wr.opcode = IBV_WR_SEND;
    wr.send_flags = IBV_SEND_INLINE;
    sge.addr = (uintptr_t) &msg;  // 4-byte message
    sge.length = sizeof(msg);
    ret = ibv_post_send(qp, &wr, &bad);
}
```

### Credit-Based Flow Control

**Problem:** The sender RDMA-writes directly into the receiver's ring buffer. Without flow control, the sender could overwrite unread data.

**Solution:** Dual mechanism — sequence numbers + SGL updates:

```
Sender tracks:
  sseq_no     — next sequence number to send
  sseq_comp   — highest sequence number the receiver has acknowledged
  → Can send when: sseq_no != sseq_comp (has credits)

Receiver tracks:
  rseq_no     — number of messages consumed
  rseq_comp   — sequence number at which next credit update is due
  → Sends credits when: rseq_no >= rseq_comp (consumed half the credits)
```

**Credit replenishment (`rs_send_credits`):**
1. Receiver consumes messages, advancing `rseq_no`
2. When `rbuf_bytes_avail >= rbuf_size/2` **or** `rseq_no >= rseq_comp`:
   - Builds a new `rs_sge` with: addr = `rbuf[rbuf_free_offset]`, key = `rmr->rkey`, length = `rbuf_size/2`
   - Posts `RDMA_WRITE_WITH_IMM` (or WRITE + SEND for iWARP) to sender's `remote_sgl`
   - `imm_data = RS_OP_SGL | (rseq_no + rq_size)` — grants new credits
3. Sender sees RS_OP_SGL completion → updates `sseq_comp`, gains new target SGL entry

**Ring buffer SGL cycling:**
- `target_sgl[]` has 2 entries (RS_SGL_SIZE = 2), cycling between halves of rbuf
- Each half is `rbuf_size/2` (default 64KB)
- Sender writes into current SGL entry, advances pointer; when exhausted, moves to next entry
- Receiver refills the consumed half and sends new SGL entry via RDMA Write to sender's `remote_sgl`

### CQ Processing and Locking

rsocket uses **two locks** for CQ access:

```
cq_lock      — protects ibv_poll_cq() and completion processing
cq_wait_lock — serializes blocking on ibv_get_cq_event()
```

This design allows `rsend` and `rrecv` to run concurrently:

```c
rs_process_cq(rs, nonblock, test):
  acquire(cq_lock)
  loop:
    rs_update_credits()       // send credit updates if needed
    rs_poll_cq()              // poll CQ for completions
    if test(rs) → break       // condition met (e.g., can_send, have_rdata)
    if nonblock → EWOULDBLOCK
    if !cq_armed:
      ibv_req_notify_cq()    // arm for next event
      cq_armed = 1
    else:
      acquire(cq_wait_lock)  // serialize waiters
      release(cq_lock)       // let other thread poll while we wait
      ibv_get_cq_event()     // block on comp_channel fd
      release(cq_wait_lock)
      acquire(cq_lock)       // re-acquire to process
  release(cq_lock)
```

**Busy-poll optimization (`rs_get_comp`):**
```c
rs_get_comp(rs, nonblock, test):
  start_time = now()
  do:
    rs_process_cq(rs, nonblock=1, test)  // try non-blocking first
    if success → return
    poll_time = now() - start_time
  while (poll_time <= polling_time)        // default: 10µs busy-poll
  rs_process_cq(rs, nonblock=0, test)      // then block on CQ event
```

### rpoll — Polling Multiple rsockets

`rpoll()` translates rsocket fds to internal kernel fds for `poll()`:

```
rpoll(fds, nfds, timeout):
  Phase 1 — Busy-poll (10µs):
    rs_poll_check() for each fd:
      For connected rsockets: poll CQ, check rdata/can_send
      For listening: poll accept_queue[0]
      For non-rsocket fds: regular poll(fd, 1, 0)

  Phase 2 — Block on kernel poll():
    rs_poll_arm() remaps fds:
      connected rsocket → cm_id->recv_cq_channel->fd  (CQ event fd)
      pre-connected     → cm_id->channel->fd           (CM event fd)
      non-rsocket       → pass through unchanged
    poll(remapped_fds + pollsignal_fd, nfds+1, timeout)

  Phase 3 — Process events:
    rs_poll_events(): for each fd with revents, get CQ event and re-check state
```

**pollsignal mechanism:** An `eventfd` that wakes all polling threads when any rsocket state changes. Required because a CQ event on one fd may update state relevant to a thread polling a different fd.

### Graceful Disconnect

```
rshutdown(socket, how):
  1. Set to blocking mode (disable O_NONBLOCK temporarily)
  2. Send control message: RS_CTRL_DISCONNECT or RS_CTRL_SHUTDOWN
     via rs_post_msg() (RDMA_WRITE_WITH_IMM with 0-byte data, or inline SEND)
  3. Wait for all outstanding sends to complete:
     rs_process_cq(rs, blocking, rs_conn_all_sends_done)
  4. If disconnected: flush CQ, call ucma_shutdown()
  5. Restore O_NONBLOCK if it was set
```

### riowrite/rioread (IOMAP-based Remote I/O)

Different from rsend/rrecv — uses pre-registered memory mappings:

```
Setup:
  riomap(socket, buf, len, PROT_WRITE, ..., offset)
    → ibv_reg_mr(pd, buf, len, LOCAL_WRITE | REMOTE_WRITE)
    → Queue iomap entry for transmission to peer
    → On next rsend/riowrite: send iomap SGL to peer via RDMA Write

riowrite(socket, buf, count, offset, flags):
  1. Send any pending iomaps
  2. Find matching iomap entry for offset
  3. Copy data into sbuf (same ring buffer as rsend)
  4. Post IBV_WR_RDMA_WRITE (no immediate data, no doorbell)
     → addr = iomap.sge.addr + (offset - iomap.offset)
     → rkey = iomap.sge.key
  5. No flow control notification — receiver must poll or use other mechanism
```

**Key difference from rsend:** riowrite uses pure `IBV_WR_RDMA_WRITE` (no immediate data), so the receiver gets **no CQ completion**. The receiver must use a separate mechanism to know data arrived.

### Key Constants and Defaults

| Constant | Value | Meaning |
|----------|-------|---------|
| `RS_OLAP_START_SIZE` | 2048 | Initial transfer size (doubles each iteration) |
| `RS_MAX_TRANSFER` | 65536 | Maximum single RDMA write size |
| `RS_SNDLOWAT` | 2048 | Minimum send buffer availability |
| `RS_QP_CTRL_SIZE` | 4 | Number of outstanding control messages |
| `RS_SGL_SIZE` | 2 | Ring buffer SGL entries (2 halves) |
| `RS_CONN_RETRIES` | 6 | CM connection retry count |
| `def_sqsize` | 384 | Default send queue depth |
| `def_rqsize` | 384 | Default recv queue depth |
| `def_mem` | 128KB | Default recv buffer size |
| `def_wmem` | 128KB | Default send buffer size |
| `def_inline` | 64 | Default inline data size |
| `polling_time` | 10µs | CQ busy-poll duration before blocking |

### Lessons for Our Stream Design

1. **Ring buffer with 2 SGL entries works** — Allows continuous writing while receiver reclaims the other half. Our stream could adopt a similar two-half scheme.

2. **Overlapped send sizing** — Starting at 2KB and doubling to 64KB amortizes small-message overhead while keeping latency low for initial data.

3. **Single CQ is simpler but has contention** — rsocket uses one CQ with `cq_lock` + `cq_wait_lock`. Our dual-CQ approach (separate send_cq/recv_cq) avoids this but costs an extra comp_channel.

4. **iWARP needs two WRs per message** — Any Write+Imm design must fall back to Write + inline Send on iWARP, doubling SQ consumption. Our Send/Recv approach avoids this entirely.

5. **Credit flow is complex but necessary** — Even with RNR retry, credits prevent ring buffer overrun. For Send/Recv, implicit flow control via recv buffer count is simpler.

6. **Private data exchange is elegant** — Embedding buffer addresses and rkeys in CM connect/accept private data avoids an extra round trip. Max 56 bytes (IB) or 196 bytes (iWARP).

7. **Busy-poll → block pattern** — The 10µs busy-poll before blocking on CQ event is a good latency optimization. Our AsyncCq could adopt a similar strategy with `tokio::task::yield_now()`.

8. **The 512MB immediate data limit** — 29-bit data field caps a single "message" at 512MB. For a general stream, chunking at 64KB is more practical.

## References

- [rsocket(7) man page](https://manpages.debian.org/testing/librdmacm-dev/rsocket.7.en.html) — API and design overview
- [rdma-core rsocket source](https://github.com/linux-rdma/rdma-core/blob/master/librdmacm/rsocket.c) — Full implementation (~3400 lines)
- [RB²: Distributed Ring Buffer](https://jokerwyt.github.io/rb2.pdf) — Write-based ring buffer design (academic)
