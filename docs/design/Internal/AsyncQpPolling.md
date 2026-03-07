# AsyncQp Dual-CQ Polling Refactor

`AsyncQp` now owns dual CQs and exposes poll-based accessors, so
`AsyncRdmaStream` reuses it instead of duplicating raw verb calls.

## Status

**Implemented**

## Problem

`AsyncRdmaStream` duplicated RDMA verb mechanics (`ibv_post_send/recv`,
CQ polling) that `AsyncQp` already wrapped, because `AsyncQp` only offered
async methods while the stream needs poll-based methods for
`futures_io::AsyncRead/AsyncWrite`.

Worse, the old `AsyncQp` used a **single shared CQ** — a silent correctness
bug. `poll_wr_id()` discards non-matching completions, so concurrent send +
recv on the same QP would silently drop completions, causing hangs. Tests
only passed because send/recv used different endpoints (different QPs/CQs).

## Design

### Dual CQs

`AsyncQp` always owns separate `send_cq` and `recv_cq`. There is no shared
CQ mode — every send-side verb (`send`, `write_remote`, `read_remote`,
atomics) polls `send_cq`, and `recv` polls `recv_cq`.

### Poll-Based Accessors

`AsyncQp` exposes composable primitives for poll-based callers:

- `poll_send_cq()` / `poll_recv_cq()` — thin `#[inline]` wrappers over
  `AsyncCq::poll_completions()`
- `post_recv_buffer(mr, wr_id)` — post a recv WR without waiting

`AsyncRdmaStream` composes these: `poll_write` uses `post_send_signaled` +
`poll_send_cq`, `poll_read` uses `poll_recv_cq` + `post_recv_buffer`.

### CQ Factory

`AsyncCq::create_tokio(ctx, depth)` creates CompletionChannel + CQ +
TokioCqNotifier in one call, replacing 4 lines of boilerplate.

## Architecture

```
┌───────────────────────────────────────────┐
│             AsyncRdmaStream               │
│  ┌────────┐ ┌─────────┐                  │
│  │send_mr │ │recv_mrs │  (buffer mgmt)   │
│  └───┬────┘ └────┬────┘                  │
│      │           │                        │
│  ┌───┴───────────┴────────────────────┐  │
│  │            AsyncQp                  │  │
│  │  ┌──────────────┐                  │  │
│  │  │ CmQueuePair  │                  │  │
│  │  └──────┬───────┘                  │  │
│  │  post_send_signaled()               │  │
│  │  post_recv_buffer()                 │  │
│  │  ┌─────────┐  ┌─────────┐          │  │
│  │  │send_cq  │  │recv_cq  │          │  │
│  │  └─────────┘  └─────────┘          │  │
│  │  poll_send_cq()  poll_recv_cq()    │  │
│  └────────────────────────────────────┘  │
│                                           │
│  CM disconnect detection (stream-only)    │
└───────────────────────────────────────────┘
```

`AsyncRdmaStream` owns a single `AsyncQp` field (replacing the old
`cmqp` + `send_cq` + `recv_cq` triple). Buffer management, CM disconnect
detection, and the `poll_close` shutdown protocol remain in the stream.

## API Changes

| Change | Detail |
|--------|--------|
| `AsyncQp::new(qp, send_cq, recv_cq)` | Breaking: 3-arg constructor (was 2-arg) |
| All async verbs | Route to correct CQ (`send_cq` or `recv_cq`) |
| `AsyncQp::poll()` | Removed — ambiguous with dual CQs |
| `poll_send_cq()` / `poll_recv_cq()` | New poll-based CQ access |
| `post_recv_buffer()` | New: post recv WR without waiting |
| `AsyncCq::create_tokio()` | New factory method |
| `post_send_raw()` / `post_recv_raw()` free fns | Removed from `async_stream.rs` |

## Non-Goals

- Changing the stream protocol (still SEND/RECV with pre-posted buffers)
- Making `AsyncQp` do buffer management (stays in `AsyncRdmaStream`)
