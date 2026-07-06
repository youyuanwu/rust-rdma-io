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
make read-ring gRPC reliable at modest aggregate load. **The *bidirectional*
flow-control deadlock is now FIXED** by a sixth change — a doorbell-blocked
RDMA-Read *heartbeat* (Status change 6) — validated cross-VM on MANA with a
reboot-clean A/B; the benchmark's read-ring gRPC `in_flight = 1` cap has been
**lifted**.

## Status

**RESOLVED (2026-07-06): the bidirectional flow-control deadlock is fixed by a
doorbell-blocked RDMA-Read heartbeat (Status change 6); the read-ring gRPC
`in_flight = 1` cap has been LIFTED.** Changes 1–5 (RNR root cause, the
*unidirectional* send-CQ lost wakeup, and the write-blocked recv-waker gap) made
read-ring gRPC reliable only at modest load and merely *mitigated* the residual
bidirectional deadlock; change 6 *closes* it. The config the benchmark used as
its canonical deterministic wedge — `conn=4 in_flight=8 payload=256` — now
passes. A **reboot-clean cross-VM A/B on MANA RoCEv2** measured **5/5 pass with
the heartbeat vs 3/3 warmup-wedge without** (same binary except the one
`send_gather` branch; A/B/A confirmed causation), and a boundary sweep up to
`conn=16 in_flight=64 payload=1024` found **0 deadlocks**. One caveat is *perf,
not correctness*: read-ring at deep pipelines with larger payloads (e.g.
`conn=8 in_flight=32 payload=1024`) collapses to a low-throughput
byte-ring-saturation regime (Read head-refresh RTT bubbles) — prefer
send-recv/credit-ring there. `pipelined_transfer_read_ring` remains green (20/20).

Six transport/stream changes landed on top of the original fix A:

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
   stash is full on both peers — see "Bidirectional gRPC stall" below. (Change 6
   is what finally eliminates it.)
6. **Doorbell-blocked RDMA-Read heartbeat — the actual deadlock fix.** The
   doorbell-blocked backpressure branch of `send_gather` (`sq_used +
   SEND_COPY_MAX_WRS > max_outstanding`) used to return `Ok(0)` with **no
   outstanding send-CQ work**: the pinned Write+Imm RNR-stall on the peer's spent
   doorbells, so a write-blocked sender parked with **no wakeup source** (the
   silent-park seed). It now posts a single one-sided RDMA Read (guarded by
   `!read_in_flight`; fits the SQ headroom). The Read always completes at
   Read-RTT regardless of the peer, so its send-CQ completion **re-polls the
   stream's `poll_write`**, which **re-runs fix A's write-blocked recv-drain**
   (repost doorbells → free the peer): the silent park becomes a self-sustaining
   relief pump, so a symmetric write-block self-unwinds instead of wedging. This
   is what actually breaks the bidirectional deadlock (changes 1–5 only mitigated
   it). The earlier belief that this was "liveness only, not a cure" was
   disproved by the reboot-clean A/B (5/5 pass vs 3/3 wedge). Guarded by the
   deterministic mechanism test
   [`read_ring_doorbell_blocked_posts_read_heartbeat`](../../rdma-io-tests/tests/read_ring_transport_tests.rs)
   (a doorbell-blocked `send_copy` must leave a Read in flight).

The fixes landed incrementally: changes 4–5 removed *lost-wakeup* stalls (a
unidirectional send-CQ reap and a bidirectional recv-CQ gap) and made read-ring
gRPC reliable at modest load; change 6 (the heartbeat) then closed the residual
*bidirectional* flow-control deadlock at higher load. Two detailed forensic
captures are kept below: the cross-VM
[Bidirectional gRPC stall](#bidirectional-grpc-stall-recv-cq-lost-wakeup-fixed--residual-flow-control-deadlock)
and the
[MANA unidirectional `pipelined_transfer` reproduction](#mana-unit-test-reproduction-pipelined_transfer-unidirectional-stall).

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

The deadlock is fixed (changes 1–6). The transport still has the underlying
write-completion-coupled-to-peer-read property — the doorbell-blocked heartbeat
pumps *around* it rather than removing it (so the transport-level coupling test
still shows the property by design). The items below are optional
*architecture/perf* reworks that would remove the coupling and the
backpressure-path copy — no longer needed for correctness:

1. **Guarantee doorbell headroom ≥ SQ depth** and repost doorbells eagerly
   (independent of stream read progress), so Write+Imm never RNR-stalls on a
   drained doorbell pool. Requires growing the `max_outstanding`-sized recv
   tracking (`virtual_idx_map`, `recv_tracker`) so doorbell reposting can be
   decoupled from slot release.
2. **Bound the stream's outstanding sends** below the point where a symmetric
   write-block can starve the peer's reposting, giving the read side room to run.
3. The lost-wakeup fixes themselves (changes 3–6) are already landed and
   validated — this section is only the optional coupling-removal rework, which
   overlaps with options B and C in the design analysis below.

## Design analysis: why it deadlocks, and the fix options

This section records the mechanism and the fix trade-offs worked out while
root-causing the bidirectional stall. The fix that actually landed — the
doorbell-blocked Read heartbeat (change 6) — is a lightweight send-side variant
of the §1 mitigation; the A/B/C options below were the pre-fix map and are kept
for context and possible future perf work.

### 1. Two backpressure paths — only one has a wakeup

`send_copy` returns `Ok(0)` (backpressure) on two distinct conditions, and they
behave very differently:

- **Doorbell-blocked** — `send_in_flight + read_in_flight + SEND_COPY_MAX_WRS >
  max_outstanding`. The sender already has as many Write+Imm in flight as the
  peer has pre-posted doorbell recv WRs to catch them. This path returns `Ok(0)`
  **without posting an RDMA Read**, so it leaves **no send-side heartbeat**. Its
  only unblock is a send completion — a Write+Imm being caught by a peer doorbell
  — which requires the peer to be *reaping and reposting doorbells* (draining).
  This is the deadlock seed: a doorbell-blocked writer whose peer has stopped
  draining parks with no wakeup (the capture signature: `rif = 0`, high stale
  `send_in_flight`).
- **Byte-blocked** — `remote_free_space() < data_len + min_free_threshold`. The
  peer's recv **ring** is full. This path **posts an RDMA Read** of the peer's
  head; the Read is one-sided and always completes, so the send CQ keeps firing
  and the task keeps being re-polled. Benign.

Which one you hit first depends on message size vs `ring_capacity /
max_outstanding` (≈ 1.5 KB default): small messages exhaust the ~43 doorbell
slots first (doorbell-blocked); large messages fill the byte ring first
(byte-blocked). **Fix (IMPLEMENTED — Status change 6):** post a Read on the
doorbell-blocked path too. The doorbell-blocked sender keeps a heartbeat; its
send-CQ completion re-polls the write path and pumps fix A's recv-drain, which is
what breaks the deadlock (measured 5/5 pass vs 3/3 wedge, reboot-clean A/B).

### 2. Why credit-ring doesn't silent-park: push relief *with a wakeup*

Both ring transports relieve backpressure only when the receiver consumes
(`repost_recv`). The difference is **how the sender learns of the relief**:

- **credit-ring** — `repost_recv` sends a credit update as a `Send+Imm` to the
  sender; it lands on the **sender's recv CQ**, fires a completion event, and
  **wakes the sender's task** (`drain_recv_credits` → `remote_credits += delta`).
  The relief *is* a notification, so a doubly-write-blocked pair keeps waking each
  other and stays live.
- **read-ring** — `repost_recv` advances the head into a local atomic offset
  buffer. That is **silent**: no message, no CQ event on the sender. The sender
  must **pull** it via an RDMA Read (a send-side op), and when doorbell-blocked it
  posts no Read → no wakeup → both peers go quiet. Silent park.

So read-ring's flow control is **pull-based and silent**; credit-ring's is
**push-based and self-notifying**. This is the crux of "make read-ring behave
like credit-ring." (Caveat: credit-ring also grants credits only at consume and
stashes bounded, so it is not *obviously* immune to the same stash-full ceiling.
**Resolved 2026-07-06:** credit-ring was measured at that config and up through
`conn=8 in_flight=16 payload=1024` — all 0 errors, no wedge; it is empirically
deadlock-free there.)

### 3. Why hyper stops servicing reads while write-blocked

An HTTP/2 connection is driven by one `h2` task whose poll loop flushes pending
writes *before* processing reads, propagating `Poll::Pending` from the flush via
`ready!`. When the transport's write half returns `Pending` (our `send_copy`
fully backpressured), the connection future short-circuits at the flush and never
reaches the read/dispatch step; it parks on the **write** waker. Reads resume only
when writes become writable again. This is correct and efficient on **TCP**, where
write-readiness self-heals (the kernel drains the socket buffer regardless of
reads). read-ring **violates that assumption**: its write-readiness is coupled to
the peer *reading* (reposting doorbells), so "park until writable" can be
permanent. send-recv/credit-ring don't couple write-completion to peer reads, so
they satisfy hyper's assumption.

### 4. Fix options considered (pre-fix map)

Four families were mapped before change 6 landed; kept for context:

- **A1 — application window** (bound unacked RPCs via h2 `INITIAL_WINDOW_SIZE` or
  an in-flight cap; `in_flight = 1` is the degenerate case). Deadlock-free but
  caps pipeline depth (Little's law).
- **A3 — transport send window** (a `send_copy` semaphore on outstanding
  Write+Imm). **Tried, measured *not* to help** — `window=2` and `window=8` both
  wedged at `conn=4 in_flight=8 payload=256` — and removed: the deadlock is a
  *wakeup* problem, not a send-*depth* one, so shrinking the window only makes
  both peers block sooner.
- **B — untangle doorbell reposting from consume** (repost past the stash cap so
  backpressure becomes byte-based, which has the Read heartbeat). Raises the
  ceiling but `virtual_idx_map` (~43 slots) still bottoms out for small messages.
- **C — dedicated reader task** that always drains recv independent of the write
  path (the "behave like credit-ring" rework, feasible via read-ring's dual CQs).
  Removes write-gating but is bounded by its channel size.

**Outcome.** None of A1/A3/B/C was needed: the deadlock was broken by the §1
mitigation — the doorbell-blocked Read heartbeat (change 6) — which is cheap (one
one-sided Read on the backpressure path) and required neither an application
depth cap (A1) nor a transport rework (C). A3 (a transport send window) was
implemented and measured *not* to help (see §4) and was removed. B/C remain as
optional perf/architecture ideas. Caveat (perf, not correctness): read-ring still
trails send-recv/credit-ring at large payloads × deep pipelines, so steer those
there.

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

**The stash-full ceiling change 5 left — and how change 6 closed it.** Change 5
still stopped draining when the recv stash filled (`MAX_STASH_BUFS`) *without*
reaching `Pending`, so the recv waker went unregistered again while the peer's
sends RNR-stalled on un-reposted doorbells — the **fundamental bidirectional
flow-control deadlock** (both peers write-block with full stashes, neither reads),
reproducing deterministically at `conn=4 in_flight=8 payload=256`. A stash-full
*self-wake* was tried and rejected (it livelocks — the connection task busy-polls
and starves the RPC tasks that would drain the stash).

**Change 6 (the doorbell-blocked Read heartbeat) fixes it.** It is *not* the
rejected self-wake: the wakeup is a real hardware Read completion (Read-RTT paced,
not a busy spin), and on each wake the stream's `poll_write` re-runs fix A's
recv-drain — reposting doorbells and freeing the peer (a relief pump, not an empty
wake). Reboot-clean cross-VM A/B: `conn=4 in_flight=8 payload=256` **5/5 pass
with the heartbeat vs 3/3 warmup-wedge without**; boundary sweep to
`conn=16 in_flight=64 payload=1024` — 0 deadlocks. The `in_flight = 1` cap is
**lifted**.

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
`pipelined_transfer_read_ring` is now green (20/20 on MANA). This fixed the
unidirectional stall; the *bidirectional* gRPC deadlock at `in_flight > 1` was a
separate coupling, later closed by change 6 (the doorbell-blocked heartbeat), so
the read-ring `in_flight = 1` bench cap is now lifted.

**Note on the full MANA suite.** Every other integration test passes on MANA.
Memory-Window, atomics, and siw/rxe-only tests self-skip via the `require_*`
macros ([lib.rs](../../rdma-io-tests/src/lib.rs)); the ring transports auto-detect
`max_mw = 0` and fall back to the MR rkey (`MemoryWindowMode::Auto`).
`pipelined_transfer_read_ring` is the sole remaining flake, and it is this stall.
