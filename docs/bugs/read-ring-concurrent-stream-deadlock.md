# ReadRing: Concurrent-Stream Deadlock over gRPC (in_flight > 1)

## Summary

`ReadRingTransport` deadlocked when used as an `AsyncRdmaStream` under
**sustained bidirectional concurrent streaming** — specifically the gRPC/HTTP-2
path (`--mode rh2 --transport read-ring`) with more than one concurrent RPC per
connection (`--in-flight > 1`). Both peers wedged with their send queues full and
never recovered. `send-recv` and `credit-ring` do **not** have this problem and
pipeline cleanly to the same depths.

Resolved at the stream layer by fix A (see Status / "Fix applied"); the
transport-level coupling that made it possible is documented below and left for a
future transport rework.

## Status

**Stream-layer fix applied (fix A); transport-level rework still deferred.**
`AsyncRdmaStream`'s write-blocked path now drains its recv CQ and *releases the
slot immediately* — copying the bytes into an owned stash and calling
`repost_recv` — so a write-blocked endpoint keeps replenishing its peer's
doorbells instead of holding them. This breaks the symmetric-write-block cycle at
the stream layer without changing the transport. See "Fix applied (fix A)" below.

The benchmark client still caps read-ring gRPC to `in_flight = 1` (with a
warning); see [`run_channel_clients`](../../tests/rdma-io-bench/src/client.rs).
That cap is retained pending validation of fix A on MANA RoCEv2 hardware — once
confirmed there it can be lifted. read-ring at `in_flight = 1` and the direct
echo path (`--mode echo`) were never affected.

This is **not a regression**: before concurrent RPCs were wired into the gRPC
client, that path only ever issued one outstanding RPC per connection, so
read-ring was always effectively `in_flight = 1` there.

## Reproduction

```bash
# Deterministic hang (0/6) at 8x8, in_flight=4:
just run-bench mode=rh2 transport=read-ring connections=8 threads=8 in_flight=4 \
    duration=8 payload=64 mw_fallback=true
```

The client connects all 8 connections, completes warmup, then hangs in the
benchmark phase (the harness kills it on timeout). Non-monotonic in the depth:
`in_flight` 1 and 2 pass, 4 deadlocks reliably (0/6), 8 sometimes passes — a
timing/interleaving dependence, not a clean threshold.

| in_flight (8×8, read-ring, rh2) | result |
|---|---|
| 1 | pass (~52k rps) |
| 2 | pass (~94k rps) |
| 4 | **deadlock (0/6)** |
| 8 | pass observed once (~122k rps) |

## Root cause

Captured with debug tracing on both peers; at the freeze **both** the client and
the server are pinned at the same state:

```
rr: send_copy Ok(0) sq-backpressure sq_used=44 cap=46 rif=false
```

i.e. the **send queue is full** (`send_in_flight = 44`, `sq_capacity = 46`), no
RDMA-Read is outstanding (`rif=false`, so this is *not* the head-refresh path),
and `send_in_flight` never drains. The dominant blocking reason by far is
SQ-backpressure, not ring-full (measured ~9136 vs ~320 events before the freeze).

It is a **bidirectional flow-control deadlock** rooted in read-ring's data-plane
mechanics:

1. Each `send_copy` posts an **RDMA Write + Immediate** into the peer's recv
   ring. The immediate must be consumed by a **doorbell recv WR** the peer has
   pre-posted; the peer replenishes doorbells only while it is *draining its own
   recv ring* — i.e. inside `poll_recv` / `repost_recv`, which run on the
   **read** side of the stream.
2. With many concurrent HTTP-2 streams, both peers fill their send queues
   (`send_in_flight → ~44`). Once a peer is write-blocked, the h2 connection
   stops servicing reads on that peer.
3. Neither peer reads ⇒ neither reposts doorbells ⇒ the in-flight Write+Imm
   operations **RNR-stall** and never complete ⇒ `send_in_flight` stays pinned
   ⇒ both tasks park with **no wakeup source** (send CQ won't fire because
   writes can't complete; recv CQ won't fire because the recv rings were just
   drained and the peer can't send anything new).

So write completion is transitively coupled to the peer's *read* progress, and a
symmetric write-block breaks the cycle.

## Why only read-ring, and only the gRPC path

- **send-recv / credit-ring don't couple the same way.** credit-ring's receiver
  *pushes* credits (Send+Imm on the recv CQ) as it drains, and send-recv posts a
  bounded recv-buffer pool; neither makes a sender's *write completion* depend on
  the peer running its stream *read* side at that instant. Both pipeline cleanly
  at `in_flight = 16` (measured).
- **Echo (`--mode echo`) is unaffected** because its client/server loops poll
  send-completions and recvs in a *single combined* `poll_fn` and retry
  `send_copy` after any progress, so a write-block still services reads. The
  gRPC path splits `poll_read` and `poll_write` across the h2 connection, which
  lets both sides get write-blocked simultaneously.
- Needs **sustained** bidirectional load, which is why warmup sometimes escapes
  and the failure is timing-dependent in the depth.

## Investigation dead-ends (recorded so we don't repeat them)

- **"Hidden progress on the RDMA-Read completion."** `poll_send_completion`
  returns `Pending` for a Read-only completion batch even though it refreshed the
  head. A stream-side "retry `send_copy` after `Pending`" did **not** fix it —
  the deadlock is on the SQ-backpressure path (`rif=false`), not the Read path.
- **Dual-poller `req_notify` desync.** `send_copy`'s `drain_send_cq_for_read`
  arms the send CQ (`req_notify`) independently of the async `CqPollState`.
  Removing that arm did **not** fix it either (same reason — wrong path). It may
  still be a latent hazard worth cleaning up alongside the real fix.

## Fix applied (fix A — stream-layer eager slot release)

The root cause reduces to one fact: a read-ring recv slot (and its doorbell) is
released only by `repost_recv`, which `AsyncRdmaStream` called *only* from
`poll_read`. A write-blocked endpoint stopped calling `poll_read`, so it held its
slots and starved its peer's doorbells.

Fix A makes the **write-blocked path itself** release the recv side, in
[`async_stream.rs`](../../rdma-io/src/async_stream.rs):

- `recv_stash` changed from a *reference* to the transport recv slot
  (`(buf_idx, byte_len)`) to **owned** pooled buffers — `VecDeque<Vec<u8>>` plus a
  `stash_offset` for partial reads and a `stash_pool` free-list to avoid
  allocating on this path.
- When `poll_write_slice` is send-blocked it drains the recv CQ, copies each
  completion's bytes into a pooled owned buffer, and calls `repost_recv`
  **immediately** — releasing the transport slot + doorbell — bounded by
  `MAX_STASH_BUFS` (once the stash is full it stops draining and lets the
  transport's own byte-backpressure apply). `poll_read` serves the owned stash
  FIFO before polling the transport, preserving byte order.

Because the release happens *synchronously inside the blocked poll, before the
task parks*, a write-blocked A frees B's flow control → B's stalled sends
complete → B's send-CQ waker (already armed) fires → B drains and releases → A's
sends complete. The circular wait becomes a self-unwinding cascade. The change is
transport-agnostic and harmless for send-recv/credit-ring.

**Cost.** One extra copy per message, of order the message size (64 B in the
repro), **only on the backpressure path** — `AsyncRdmaStream` is already a
copying adapter (send and read both copy), and steady-state reads are unchanged.
A copy-free alternative was rejected: enlarging the doorbell pool only converts
the doorbell deadlock into a byte-space stall under sustained load, and bounding
stream depth caps throughput without guaranteeing deadlock-freedom.

**Tests.**
- `read_ring_write_completion_coupled_to_peer_read`
  ([read_ring_transport_tests.rs](../../rdma-io-tests/tests/read_ring_transport_tests.rs))
  — transport-level characterization of the coupling fix A works around. It still
  reproduces the exact freeze signature (44 sends pinned until the peer reads) and
  documents *why* the stream-layer release is needed; the transport itself is
  unchanged.
- `concurrent_write_no_deadlock_read_ring`
  ([async_stream_tests.rs](../../rdma-io-tests/tests/async_stream_tests.rs)) —
  stream-level regression: both peers write 50 small messages before either reads
  (the h2 write-priority pattern). Pre-fix this deadlocks; with fix A both writers
  complete and the buffered streams verify in order. Validated deterministically
  on rxe.

## Remaining transport-level work (deferred)

Fix A resolves the deadlock at the stream layer but leaves the transport's
write-completion-coupled-to-peer-read property in place (hence the transport-level
repro still wedges). A deeper transport rework could remove the coupling — and the
backpressure-path copy — entirely. Candidate approaches:

1. **Guarantee doorbell headroom ≥ SQ depth** and repost doorbells eagerly
   (independent of stream read progress), so Write+Imm never RNR-stalls on a
   drained doorbell pool. Requires growing the `max_outstanding`-sized recv
   tracking (`virtual_idx_map`, `recv_tracker`) so doorbell reposting can be
   decoupled from slot release.
2. **Bound the stream's outstanding sends** below the point where a symmetric
   write-block can starve the peer's reposting, giving the read side room to run.
3. Revisit the `poll_send_completion` "Read-only ⇒ Pending" hiding and the
   `drain_send_cq_for_read` dual-arm as part of the same change.

Before lifting the bench `in_flight = 1` cap, validate fix A at `in_flight`
4/8/16 on the MANA RoCEv2 preview (the CM is flaky there, so use
`reboot_between`/retries).
