# ReadRing: Concurrent-Stream Deadlock over gRPC (in_flight > 1)

## Summary

`ReadRingTransport` deadlocks when used as an `AsyncRdmaStream` under **sustained
bidirectional concurrent streaming** — specifically the gRPC/HTTP-2 path
(`--mode rh2 --transport read-ring`) with more than one concurrent RPC per
connection (`--in-flight > 1`). Both peers wedge with their send queues full and
never recover. `send-recv` and `credit-ring` do **not** have this problem and
pipeline cleanly to the same depths.

## Status

**Open — worked around, not fixed.** The benchmark client caps read-ring gRPC to
`in_flight = 1` (with a warning) so runs don't hang; see
[`run_channel_clients`](../../tests/rdma-io-bench/src/client.rs). The transport
fix is deferred. read-ring at `in_flight = 1` is unaffected, and the direct echo
path (`--mode echo`) is unaffected (see "Why only the gRPC path" below).

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

## Fix direction (deferred)

The real fix is read-ring flow-control rework so that write completion does not
depend on the peer's instantaneous read progress. Candidate approaches:

1. **Guarantee doorbell headroom ≥ SQ depth** and repost doorbells eagerly
   (independent of stream read progress), so Write+Imm never RNR-stalls on a
   drained doorbell pool.
2. **Bound the stream's outstanding sends** below the point where a symmetric
   write-block can starve the peer's reposting, giving the read side room to run.
3. Revisit the `poll_send_completion` "Read-only ⇒ Pending" hiding and the
   `drain_send_cq_for_read` dual-arm as part of the same change.

Any fix must be validated at `in_flight` 4/8/16 on the MANA RoCEv2 preview (the
CM is flaky there, so use `reboot_between`/retries).
