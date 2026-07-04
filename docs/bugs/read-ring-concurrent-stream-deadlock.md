# ReadRing: Concurrent-Stream Deadlock over gRPC (in_flight > 1)

## Summary

`ReadRingTransport` deadlocked when used as an `AsyncRdmaStream` under
**sustained bidirectional concurrent streaming** — specifically the gRPC/HTTP-2
path (`--mode rh2 --transport read-ring`) with more than one concurrent RPC per
connection (`--in-flight > 1`). Both peers wedged with their send queues full and
never recovered. `send-recv` and `credit-ring` do **not** have this problem and
pipeline cleanly to the same depths.

Resolved at the stream layer by fix A; two transport **lost-wakeup** bugs were
then found and fixed — the *unidirectional* send-CQ reap (`send_copy` reaping the
send CQ out-of-band; Status change 4) and the write-blocked path not registering
its *recv* waker (now drains `poll_recv` to `Pending`; Status change 5). These
make read-ring gRPC reliable at modest aggregate load. **But the *bidirectional*
flow-control deadlock that fix A only mitigates is still reachable at higher
aggregate load** — both peers write-block with full stashes and neither reads —
so the benchmark keeps read-ring gRPC capped at `in_flight = 1`.

## Status

**Two lost-wakeup bugs fixed; the fundamental bidirectional flow-control deadlock
remains, so read-ring gRPC stays capped at `in_flight = 1`.** The RNR root cause,
the *unidirectional* send-CQ lost wakeup, and the write-blocked recv-waker gap are
all fixed — `pipelined_transfer_read_ring` is green (20/20) and the cross-VM gRPC
bench is stall-free at modest load (validated `conn≤8 in_flight≤8`, 64 B, 0 stalls,
≈113k rps at `conn=8 in_flight=4`). **However, the bidirectional deadlock still
reproduces at higher aggregate load:** `conn=4 in_flight=8 payload=256`
deterministically wedges in warmup on MANA RoCEv2 (both peers write-block with
full recv stashes, neither reads). The safe boundary depends on
connections × depth × payload, so the benchmark keeps the conservative
`in_flight = 1` cap; use send-recv or credit-ring for pipelined gRPC.

Five transport/stream changes landed on top of the original fix A:

1. **Doorbell reposting decoupled from application reads.** `poll_recv` now
   reposts the doorbell recv WR at the moment it *reaps* a data completion
   (mirroring the padding branch), instead of only in `repost_recv` when the
   app consumes the message. Recv-ring overwrite protection stays with the
   independent RDMA-Read `head` flow control, so this is safe.
2. **Outstanding Write+Imm bounded to the peer's recv capacity.** `send_copy`'s
   backpressure guard now caps in-flight Write+Imm against `max_outstanding`
   (the peer's doorbell / virtual-index count) rather than the deeper physical
   `sq_capacity`. Previously the send queue could hold *more* Write+Imm than the
   peer had doorbells to catch (measured `sif = 44` vs 43 doorbells), so 1-2
   always RNR-stalled — the deadlock seed. This is the "doorbell headroom ≥ SQ
   depth" fix from the deferred-work list below.
3. **Single send-CQ poller.** `drain_send_cq_for_read` no longer arms the CQ
   (`req_notify`); arming is owned solely by `poll_send_completion`'s
   `CqPollState`, removing a dual-poller lost-wakeup hazard.
4. **`send_copy` no longer polls the send CQ at all (the residual-stall fix).**
   `drain_send_cq_for_read` is *removed*: `poll_send_completion` is now the
   **sole** consumer of the send CQ, including the RDMA-Read head-refresh
   completion (it reaps `WR_ID_READ_SENTINEL`, calls `update_cached_remote_head`,
   clears `read_in_flight`, and returns `Ready(Ok)` so a write-blocked caller
   re-polls `send_copy` with the refreshed head). Even without arming, the old
   out-of-band `cq.poll()` inside `send_copy` *reaped* completions behind the
   `CqPollState` state machine's back, swallowing the notification
   `poll_send_completion` was parked on — so a writer whose sends had already
   completed never woke. That is the lost-wakeup behind the sustained-load
   stall. `send_copy` is now monotonic: it only ever *posts* work, never reaps.
   Guarded by
   [`read_ring_send_copy_must_not_reap_send_cq`](../../rdma-io-tests/tests/read_ring_transport_tests.rs),
   a deterministic invariant test (asserts `send_copy` never decreases
   `sends_in_flight()`) that failed on the old code and passes after the fix.
5. **Write-blocked recv drain registers the recv waker (closes a lost-wakeup
   gap).** Fix A's write-blocked drain in `poll_write_slice` polled the recv CQ a
   single batch; when that returned `Ready` (data found) it left the recv CQ
   drained-but-unwatched (`poll_recv` registers the recv waker only on
   `Pending`), then parked on the send CQ. Under HTTP/2 hyper stops polling reads
   while the write is blocked, so the reader could park with no recv waker and a
   response completion woke nobody. Now the drain **loops until `poll_recv`
   returns `Pending`** (or the stash fills), so the recv waker is registered
   before parking. Transport-agnostic; see
   [`poll_write_slice`](../../rdma-io/src/async_stream.rs). This removes the lost
   wakeup and makes read-ring gRPC reliable at modest load, but it does **not**
   eliminate the underlying *bidirectional* flow-control deadlock once the recv
   stash is full on both peers — see "Bidirectional gRPC stall" below.

**Measured effect on MANA RoCEv2** (rh2, 64 B, `--mw-fallback`): the former
*deterministic* deadlock is gone — `conn=8 in_flight=4` now passes ~2/3
(≈106-110k rps, 0 errors) and `conn=1 in_flight=8` ~3/4 (≈40k rps), where both
were previously a hard hang. `in_flight = 1..4` are reliable. send-recv and
credit-ring are unaffected (≈112k rps at `in_flight = 8`, 0 errors).

**The unidirectional residual stall is fixed** by change 4 above. It previously
wedged with the client write-saturated (`sif ≈ 41`) and the server idle
(`sif = 0`), *not* on doorbell exhaustion (`sif ≤ 42 ≤ 43` doorbells) — a lost
send-CQ wakeup caused by `send_copy`'s out-of-band reap. The *bidirectional*
gRPC path at `in_flight > 1` is a distinct, deeper coupling: a cross-VM capture
root-caused a lost *recv*-CQ wakeup (the recv-side analog of change 4), fixed by
change 5 (draining `poll_recv` to `Pending`). That removes the lost wakeup at
modest load, but the underlying flow-control deadlock still reproduces once both
recv stashes fill (`conn=4 in_flight=8 payload=256` wedges on MANA) — see
[Bidirectional gRPC stall](#bidirectional-grpc-stall-recv-cq-lost-wakeup-fixed--residual-flow-control-deadlock)
below. So the benchmark's read-ring `in_flight = 1` cap is **retained**.

The stall reproduced on MANA in a **unidirectional** integration test
(`pipelined_transfer_read_ring`) — a lost wakeup on the send-completion path,
not a gRPC-only phenomenon; it is now green. See
[MANA unit-test reproduction](#mana-unit-test-reproduction-pipelined_transfer-unidirectional-stall)
below.

### Original fix A (superseded in part by the above)

`AsyncRdmaStream`'s write-blocked path drains a batch of its recv CQ and
*releases each slot immediately* — copying the bytes into an owned stash and
calling `repost_recv` — so a write-blocked endpoint keeps replenishing its peer's
doorbells (and advancing head) instead of holding them. This breaks the
symmetric-write-block cycle at the stream layer. See "Fix applied (fix A)" below.

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
  head. A stream-side "retry `send_copy` after `Pending`" did **not** fix *this*
  (bidirectional) deadlock — it is on the SQ-backpressure path (`rif=false`), not
  the Read path.

  **Update:** the Read-only ⇒ `Pending` behavior *was* a real bug in a different
  scenario — a **unidirectional** pipelined stream. A write-blocked sender posts
  a proactive Read as its only outstanding WR; when it completed,
  `poll_send_completion` refreshed the head (freeing remote space) but returned
  `Pending`, so the sender parked with space available and — read-ring has no
  push wakeup for a head advance — never woke. Fixed by counting a
  head-refreshing Read as progress (`Ready(Ok)`), so the blocked writer re-polls
  `send_copy`. See the commit for `read_ring_transport::poll_send_completion`.
  (The analogous credit-ring writer stall — the recv path reaping the send CQ out
  from under `poll_send_completion` — was fixed by retrying `send_copy` after the
  recv drain in `poll_write_slice`.)
- **Dual-poller `req_notify` desync.** `send_copy`'s `drain_send_cq_for_read`
  arms the send CQ (`req_notify`) independently of the async `CqPollState`.
  Removing that arm did **not** fix it either (same reason — wrong path). It may
  still be a latent hazard worth cleaning up alongside the real fix.
- **In-process unit-test repro of the *bidirectional* stall (2026-07-04).**
  Tried a single-process tonic test mirroring the bench topology: 8 h2 channels
  (each its own RDMA connection) × 4 in-flight `say_hello` loops, 96k RPCs
  sustained, no per-round barrier (the barrier'd `concurrent_unary_load` drains
  each round and never wedges; a single channel at in_flight=8 doesn't either).
  On MANA it does **not** reproduce the load stall (0 load-stalls in 15 runs);
  every intermittent failure is a read-ring **CM connect wedge** at setup
  (`ConnectError("expected Established, got Unreachable")` or a connect hang),
  orthogonal to the data path. Adding connect retries made the load phase 15/15.
  Meanwhile the **cross-VM bench** wedges *in the benchmark phase* — the client
  log shows `Connected 8 clients` → `Warmup complete` → `Benchmarking for 8s…`
  then hangs — so the stall is a genuine data-path event, but it needs real
  cross-VM RTT: same-VM loopback latency never opens the timing window. The
  bidirectional stall is therefore **not reproducible in a single-process unit
  test**; the repro is the cross-VM bench
  (`just run-bench mode=rh2 transport=read-ring connections=8 in_flight=4`).

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
3. The `poll_send_completion` "Read-only ⇒ Pending" hiding is **fixed** (a
   head-refreshing Read now returns `Ready(Ok)` so the blocked writer re-polls);
   the `drain_send_cq_for_read` dual-poller hazard is now **removed** as well —
   `send_copy` no longer polls the send CQ, so `poll_send_completion` is the sole
   owner (Status change 4). This closed the residual *unidirectional* lost-wakeup
stall. The recv-side analog (the *bidirectional* gRPC stall) is a lost recv-CQ
   wakeup, fixed at modest load by the write-blocked drain registering the recv
   waker before parking; the fundamental flow-control deadlock at high load
   remains. See "Bidirectional gRPC stall" below.

Before lifting the bench `in_flight = 1` cap, validate fix A at `in_flight`
4/8/16 on the MANA RoCEv2 preview (the CM is flaky there, so use
`reboot_between`/retries).

## Bidirectional gRPC stall: recv-CQ lost wakeup (fixed) + residual flow-control deadlock

**The residual `in_flight > 1` gRPC stall was a lost *recv*-CQ wakeup on the
reader — the recv-side analog of the send-CQ lost wakeup fixed in Status change
4 — now fixed (see "Root cause and fix" below).** Captured on MANA with the client on vm2 and the server on vm1 (rh2,
read-ring, `connections=8 in_flight=4`), both sides instrumented to dump
transport state at every backpressure/park point (temporary `RRDIAG` logging).

**Capture (both sides, 8 connections each):**

| Signal | Client | Server |
|---|---|---|
| Last log timestamp | **t ≈ 4.97 s, then silent** | t ≈ 59.7 s (polled until the kill) |
| Park point | **149× `poll_recv` Pending, 0× `poll_send_completion` Pending** | 159× `poll_recv` Pending |
| `send_in_flight` at park | 34–41 (high) | 34–41 (high) |
| `read_in_flight` | 0 | 0 |
| remote free space | 10k–64k of 65536 (**not full**) | large |

The connections wedge one-by-one (first at t ≈ 3.4 s, the rest by t ≈ 5 s), then
the **client goes fully silent** — every task parked in `poll_recv` and never woke
— while the server keeps getting polled to the 60 s timeout.

**Mechanics:**

1. The client sends its requests; `send_in_flight` climbs to ~38. Those Write+Imm
   actually *complete* at the HW (the server catches the immediates), but the
   count stays high because the client is parked in `poll_recv`, **not**
   `poll_send_completion`, so it never reaps them. The high `send_in_flight` is
   stale accounting, not a real send backlog.
2. The client then parks in `poll_recv` with the recv CQ armed, waiting for the
   responses.
3. The server posts responses (its own `send_in_flight` is high). They arrive at
   the client, its doorbell catches the immediate, and a completion lands on the
   client's recv CQ — **but that completion never wakes the parked client.**
4. The client's h2 tasks sleep forever → no new requests → the server starves
   waiting for the next request → the whole connection set wedges.

The alternatives are ruled out by the capture: `read_in_flight = 0` (not the
RDMA-Read head-refresh path), remote free space large (not ring-full — only 3
`ring-full`/`doorbell-bp` events total across the whole client run), and the park
is on the **read** side, never `poll_send_completion`.

**Why cross-VM only.** Real RTT opens the arm/park-vs-completion window: the
server's response lands *just after* the client arms the recv CQ and parks.
Same-VM loopback delivers completions synchronously enough that the drain-after-
arm always catches them before the park, which is why the in-process tonic repro
(8 conns × 4 in-flight, 96k RPCs) never reproduced it — every in-process failure
was a CM connect wedge, not this stall (see the dead-ends note).

**Root cause (confirmed) and fix.** Fix A's write-blocked drain in
[`poll_write_slice`](../../rdma-io/src/async_stream.rs) polled the recv CQ a
**single batch**. `poll_recv` registers this task's recv waker only on its
`Pending` return; when the batch came back `Ready` (data found) it left the recv
CQ drained-but-unwatched, and the path then parked on the **send** CQ
(`poll_send_completion` Pending) — registering only the send waker. Under
HTTP/2, hyper stops polling reads once the connection's write is blocked, so this
drain was the *only* thing that would have registered the recv waker. With it
returning `Ready`, the reader parked with no recv waker: the server's response
completion arrived and woke nobody. (The high stale `send_in_flight` is the
tell — the task parked on the read side while its own sends had already
completed, so `poll_send_completion` was never even reached to re-arm.)

The fix makes the write-blocked drain **loop until `poll_recv` returns `Pending`**
(or the stash fills), so the recv waker is registered before the task parks. It
is transport-agnostic and bounded by `MAX_STASH_BUFS`.

**Partially validated on MANA RoCEv2** (cross-VM, cap lifted): at modest load the
lost wakeup is gone — **0 benchmark-phase stalls** at 64 B (`conn=8 in_flight=4`
≈113k rps, double the `in_flight=1` ≈51k; `conn=1 in_flight=8` 7/7). In-process
suites unaffected: `async_stream_tests` 37/37, `read_ring_transport_tests` 4/4.

**But the fix is *not* complete.** When the recv stash fills to `MAX_STASH_BUFS`,
the drain stops (to keep backpressuring the peer) *without* reaching `Pending`,
so the recv waker is again unregistered — and at that point the peer's sends are
RNR-stalled on our un-reposted doorbells, so no recv completion will arrive to
fire even a registered waker. This is the **fundamental bidirectional
flow-control deadlock** (both peers write-block with full stashes, neither reads),
which fix A only mitigates — not a lost wakeup. It reproduces deterministically at
higher aggregate load: **`conn=4 in_flight=8 payload=256` wedges in warmup** on
MANA. A self-wake at the stash-full park was tried and rejected — it livelocks
(the connection task busy-polls, starving the RPC tasks that would drain the
stash). Because the safe boundary depends on connections × depth × payload, the
benchmark's read-ring `in_flight = 1` cap is **retained**; send-recv and
credit-ring remain the transports for pipelined gRPC. Genuinely lifting the cap
needs the flow-control coupling itself removed (see deferred work) or the h2
layer servicing reads while write-blocked.

## MANA unit-test reproduction: `pipelined_transfer` (unidirectional stall)

> **Resolved.** Root-caused to `send_copy`'s out-of-band send-CQ reap
> (`drain_send_cq_for_read`) and fixed by making `poll_send_completion` the sole
> send-CQ consumer (Status change 4). After the fix `pipelined_transfer_read_ring`
> is **20/20** on MANA (previously ~1/3 failures) and the full `async_stream_tests`
> suite is 37/37 (2×). The section below documents the original diagnosis.

The residual sustained-load stall also reproduced in the **integration tests on
MANA hardware** — it was *not* purely a gRPC/bidirectional phenomenon. Running the
suite on a MANA VM (via
[`run_unit_tests.yml`](../../tests/e2e/playbooks/run_unit_tests.yml) /
`just test-remote`), `pipelined_transfer_read_ring`
([async_stream_tests.rs](../../rdma-io-tests/tests/async_stream_tests.rs)) failed
intermittently — ~1/3 of runs. `pipelined_transfer_default` (send-recv) showed a
rarer analogous stall. **rxe/CI did not reproduce it**; it was timing-dependent
and only surfaced on the MANA preview's latencies.

The test is a **unidirectional** 500 KB blast: one task `write_all`s 512×1000 B
with no application flow control, the other reads. This is a simpler shape than
the bidirectional gRPC deadlock but the same underlying coupling.

**Observed failure sequence** (captured with disconnect-source instrumentation at
each `peer_disconnected = true` site, the CM event handler, and
`AsyncRdmaStream`'s `poll_close`/`Drop`):

1. The **reader** blocks in `read()` and hits its 30 s timeout — no data is
   arriving even though the writer is still going.
2. The reader task panics on the timeout ⇒ its stream is **dropped** ⇒
   `Drop → disconnect()` ⇒ the writer's CM channel gets a `Disconnected` event.
3. The writer's next `write_all` sees `poll_disconnect() == true` and fails with
   `BrokenPipe: "connection closed"`; the `tokio::join!` then surfaces the
   `JoinError::Panic`.

So the **primary event is the reader starving**, not a hardware WC error (no
`IBV_WC_*` error status is ever logged) and not a genuine peer close — the
"connection closed" is a *secondary cascade* from the reader-drop. The writer is
write-blocked on flow control while the server sits idle with no wakeup: the same
"client write-saturated, server idle, `sif` **not** doorbell-bound" signature as
the bench residual stall. The wakeup that was lost is the writer's **send-CQ**
notification: its Write+Imm had completed but `send_copy`'s out-of-band
`drain_send_cq_for_read` reaped the completion behind `poll_send_completion`'s
arming state machine, so the parked writer never woke, stopped posting, and the
reader — receiving no new immediates — starved.

**Test-level fixes did not work** (and were reverted):
- Keeping the reader's `server` alive past the `read()` loop (return it from the
  task instead of dropping) removes only a *secondary* end-of-transfer drop race.
- Reading to EOF instead of a fixed byte count (so the reader keeps *polling* the
  transport until the writer's graceful shutdown) improved the pass rate
  (~6/15 → ~10/15) by eliminating that drop race, but ~1/3 still stalled on the
  underlying lost wakeup.

A test could not paper over the transport lost-wakeup, so the blast was left
unchanged and the fix went into the transport: `send_copy` no longer polls the
send CQ (see Status change 4), making `poll_send_completion` the sole owner. The
deterministic guard
[`read_ring_send_copy_must_not_reap_send_cq`](../../rdma-io-tests/tests/read_ring_transport_tests.rs)
locks in the invariant (`send_copy` must never decrease `sends_in_flight()`), and
`pipelined_transfer_read_ring` is now green (20/20 on MANA). This fixes the
unidirectional stall only; the bidirectional gRPC benchmark deadlock at
`in_flight > 1` (~1/3 on MANA) is unchanged, so the read-ring `in_flight = 1`
bench cap stays until that separate coupling is resolved.

**Note on the full MANA suite.** Every other integration test passes on MANA.
Memory-Window, atomics, and siw/rxe-only tests self-skip via the `require_*`
macros ([lib.rs](../../rdma-io-tests/src/lib.rs)); the ring transports auto-detect
`max_mw = 0` and fall back to the MR rkey (`MemoryWindowMode::Auto`).
`pipelined_transfer_read_ring` is the sole remaining flake, and it is this stall.
