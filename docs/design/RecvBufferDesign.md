# Recv Buffer Design: Why Multiple Recv Buffers Are Required

## The Problem

An RDMA SEND/RECV stream needs pre-posted receive buffers. When the peer
posts an RDMA SEND, the HCA (or software provider like siw) matches it
against the next posted receive buffer on the receiver's queue pair. If no
receive buffer is available, the behavior depends on the transport:

| Transport   | Missing recv buffer behavior              |
|-------------|-------------------------------------------|
| InfiniBand  | RNR NAK → sender retries (rnr_retry_count=7 = infinite) |
| RoCE v2     | RNR NAK → sender retries                 |
| iWARP / siw | **Fatal DDP TERMINATE → connection destroyed** |

iWARP (including siw, the soft-iWARP kernel module used for testing) uses
TCP as its underlying transport. Data arrives in-order and is placed
directly into the receive buffer via DDP (Direct Data Placement). If no
receive buffer is posted, the DDP layer has nowhere to place the data and
generates a TERMINATE message:

```
siw: got TERMINATE. layer 1, type 2, code 2
```

Decoded: **DDP Untagged Buffer Error – Invalid MSN (Message Sequence Number)**.
This is a fatal, unrecoverable error that destroys the QP.

## Why One Recv Buffer Is Not Enough

With a single recv buffer, the lifecycle is:

```
post_recv(buf) → peer sends → buf consumed → app reads → re-post(buf)
                                 ↑                           ↑
                                 │  gap: no recv buffer      │
                                 └───────────────────────────┘
```

Between consumption and re-post, there is a **window with no recv buffer**.
If the peer sends another message during this window:

- **InfiniBand/RoCE**: The sender's HCA gets an RNR NAK and retries
  automatically. The retry is invisible to the application. The message is
  delivered once the receiver re-posts.

- **iWARP/siw**: The data arrives over TCP immediately. DDP finds no recv
  buffer. The connection is terminated with a TERMINATE error. **Game over.**

This window is especially dangerous with HTTP/2 (used by tonic/gRPC),
where both peers send rapid bursts of small frames:

```
Client                          Server
──────                          ──────
SETTINGS (24 bytes)    →
                       →        SETTINGS (27 bytes)
SETTINGS_ACK (13 bytes)→
                       →        SETTINGS_ACK (13 bytes)
HEADERS + DATA (55 bytes) →
```

The client sends 3 frames in rapid succession. Each is a separate RDMA
SEND. If the server has only 1 recv buffer, the second frame arrives before
the first is processed and re-posted → **DDP TERMINATE**.

## The Solution: Multiple Pre-posted Recv Buffers

We pre-post `NUM_RECV_BUFS` (currently 4) receive buffers at connection
time. Each buffer has a unique WR ID (0 through N-1) so the completion
identifies which buffer to read from and re-post:

```
WR ID 0:  recv_mrs[0]  ──── posted ────→  consumed ──→ read by app ──→ re-posted
WR ID 1:  recv_mrs[1]  ──── posted ────→  absorbs next message while [0] is being read
WR ID 2:  recv_mrs[2]  ──── posted ────→  absorbs burst
WR ID 3:  recv_mrs[3]  ──── posted ────→  absorbs burst
WR ID 4:  send_mr      ──── (send operations use this WR ID)
```

The invariant: **at least one recv buffer is always posted** while the
application processes completions from the others. With 4 buffers, up to
3 messages can arrive in a burst before the first is re-posted.

### Why 4?

HTTP/2 connection setup sends up to 3 frames from each side before reading.
4 buffers provide margin. The cost is 4 × 64 KiB = 256 KiB per connection
(or per-side), which is negligible for RDMA workloads.

The value is a compile-time constant (`NUM_RECV_BUFS`) and can be increased
if a protocol requires deeper burst absorption.

## Separate Send and Recv CQs

In addition to multiple recv buffers, the async stream uses **separate
completion queues** for send and recv operations:

```
          ┌──────────┐
 send  →  │ send_cq  │  ← poll_write waits here
          └──────────┘
          ┌──────────┐
 recv  →  │ recv_cq  │  ← poll_read waits here
          └──────────┘
```

### Why not a shared CQ?

With a shared CQ, `poll_write` might find a recv completion instead of the
send completion it needs. It can "stash" the recv completion for later, but
it cannot re-post the recv buffer (that's `poll_read`'s job). This creates
a deadlock with bidirectional protocols:

1. **Side A** calls `poll_write` → posts SEND → waits for send completion
2. **Side B** calls `poll_write` → posts SEND → waits for send completion
3. A's SEND lands in B's recv buffer → recv completion on B's shared CQ
4. B's `poll_write` sees the recv completion, stashes it, but doesn't re-post
5. B's SEND can't land on A because A consumed its recv buffer (same situation)
6. Both sides wait for send completions that never arrive → **deadlock**

With separate CQs, `poll_write` only ever sees send completions. The recv
CQ accumulates recv completions independently. No cross-contamination, no
stashing, no deadlock.

### Cost

Two extra file descriptors (one per completion channel) and two extra
`AsyncFd` registrations with tokio. Negligible.

## Resource Sizing

Each stream allocates:

| Resource         | Count | Size / Config |
|------------------|-------|---------------|
| Recv MRs         | 4     | 64 KiB each (configurable) |
| Send MR          | 1     | 64 KiB (configurable) |
| Send CQ          | 1     | depth 2 |
| Recv CQ          | 1     | depth NUM_RECV_BUFS + 1 = 5 |
| Comp Channels    | 2     | one per CQ (for async notification) |
| QP max_send_wr   | 2     | (1 active + 1 headroom) |
| QP max_recv_wr   | 5     | NUM_RECV_BUFS + 1 |

The async stream uses separate send and recv CQs (§ Separate Send and Recv CQs)
to avoid the shared-CQ deadlock.

## Implementation Details

### WR ID Convention

```
WR ID 0        → recv_mrs[0]
WR ID 1        → recv_mrs[1]
WR ID 2        → recv_mrs[2]
WR ID 3        → recv_mrs[3]
WR ID 4 (SEND) → send_mr
```

`process_recv_wc()` uses `wc.wr_id() as usize` to index into `recv_mrs[]`.
After copying data to the application buffer, the same index is re-posted.

### Partial Reads

If the application buffer is smaller than the received message,
`recv_pending` stores `(buf_index, offset, total_len)`. Subsequent
`poll_read` calls continue from the offset. The recv buffer is only
re-posted when the entire message has been consumed.

### Pre-posting Before Connection

All recv buffers are posted **before** `connect()` or `accept()` completes.
This ensures that the very first message from the peer has a buffer waiting.
On the async path, `AsyncCq` is created **after** the connection is
established, but this is safe because the drain-after-arm pattern finds
completions that arrived before the CQ was armed.

## Graceful Shutdown and QP Error Detection

When a peer disconnects, the local QP transitions to ERROR state and the
HCA flushes all outstanding work requests as `IBV_WC_WR_FLUSH_ERR`
completions. Proper detection of this state is critical to avoid hangs.

### The Problem

With separate CQs, `poll_read` and `poll_write` each wait on their own CQ.
When the peer disconnects:

1. Recv CQ receives flush completions for all posted recv buffers.
2. Send CQ receives a flush completion for any in-flight send.

However, **notification delivery is not guaranteed** due to EPOLLET
edge-triggered semantics. If the CQ event arrives between `req_notify_cq()`
and `poll_cq()` calls, it may be consumed. But if the event arrives and
the fd becomes readable between two `poll_readable()` calls where the first
already cleared the readiness flag, the second never fires — the
notification is lost.

This creates a hang scenario during HTTP/2 shutdown:

```
1. Server receives shutdown signal → h2 sends GoAway + Ping
2. poll_write posts SEND → waits for send CQ completion
3. Client drops connection → server QP enters ERROR state
4. poll_read sees flush → sets read_eof, returns EOF
5. poll_write still waiting on send CQ — notification may be lost
   → Pending forever → server task hangs
```

### Three-Layer Fix (all three phases of poll_close)

**Layer 1: `read_eof` flag.** When `poll_read` sees a flush completion,
it sets `read_eof = true`. Subsequent `poll_read` calls return `Ok(0)`
immediately — the QP will never produce another recv completion.

**Layer 2: Early exit on known-dead QP.** `poll_write` and `poll_close`
check `read_eof` at entry. If the QP is already known to be dead
(from a prior `poll_read`, `poll_write`, or disconnect detection),
they return `BrokenPipe` / `Ok(())` immediately instead of waiting
for a completion that may never arrive.

**Layer 3: `check_disconnect()` + `is_qp_dead()` safety net.** When any
poll method would return `Pending`, it probes for disconnect via two
mechanisms:

1. **CM event channel probe** (`try_get_event`): Checks the connection's
   rdma_cm event channel for a DISCONNECTED event. This is the most
   reliable signal — rdma_cm guarantees delivery regardless of provider.
   On rxe (RoCE), the QP may stay in RTS after peer disconnect until the
   CM event is consumed; CQ flush completions may not arrive or their
   EPOLLET notifications may be lost.

2. **QP state query** (`ibv_query_qp`): Falls back to checking if the QP
   has left RTS state (SQD, SQE, ERROR, or any non-RTS). This catches
   cases where the CM event hasn't arrived yet but the QP has already
   transitioned.

Applied to:
- `poll_read`: returns `Ok(0)` (EOF) when recv CQ returns `Pending`
- `poll_write`: returns `BrokenPipe` when send CQ returns `Pending`
- `poll_close` Phase 1: returns `Ok(())` when draining a pending send
- `poll_close` Phase 2: returns `Ok(())` when `disconnect()` fails on dead QP
- `poll_close` Phase 3: returns `Ok(())` when DISCONNECTED event never arrives
  (uses `is_qp_dead()` only — cannot use `check_disconnect()` here because
  `poll_expect_event` is already monitoring the same event channel)

```
check_disconnect():
  try_get_event → DISCONNECTED? → set read_eof, return true
  else → is_qp_dead()? → set read_eof, return true

poll_write(cx, buf):
  if read_eof → BrokenPipe                     # Layer 2
  post send (if not pending)
  poll send_cq:
    Pending → if check_disconnect() → BrokenPipe  # Layer 3
              else → Pending
    Ready(flush) → BrokenPipe
    Ready(ok) → Ok(len)

poll_close(cx):
  if read_eof → Ok(())                         # Layer 2
  Phase 1 (drain pending send):
    Pending → if check_disconnect() → Ok(())   # Layer 3
              else → Pending
  Phase 2 (disconnect):
    if read_eof → Ok(())                       # (set by Phase 1)
    disconnect() err → if check_disconnect() → Ok(())
  Phase 3 (await DISCONNECTED):
    Pending → if is_qp_dead() → Ok(())         # Layer 3 (QP-only)
              else → Pending

poll_read(cx, buf):
  if read_eof → Ok(0)                          # Layer 1
  poll recv_cq:
    Pending → if check_disconnect() → Ok(0)    # Layer 3
    Ready(flush) → set read_eof, Ok(0)         # Layer 1
```
