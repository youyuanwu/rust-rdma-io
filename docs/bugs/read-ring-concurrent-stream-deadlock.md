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

**RNR root cause fixed; a residual sustained-load stall remains, so the
benchmark keeps read-ring gRPC capped at `in_flight = 1`.**

Three transport/stream changes landed on top of the original fix A:

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

**Measured effect on MANA RoCEv2** (rh2, 64 B, `--mw-fallback`): the former
*deterministic* deadlock is gone — `conn=8 in_flight=4` now passes ~2/3
(≈106-110k rps, 0 errors) and `conn=1 in_flight=8` ~3/4 (≈40k rps), where both
were previously a hard hang. `in_flight = 1..4` are reliable. send-recv and
credit-ring are unaffected (≈112k rps at `in_flight = 8`, 0 errors).

**A residual stall remains at `in_flight > 1` under *sustained* load** (the
benchmark phase, after warmup): occasionally both peers wedge with the client
write-saturated (`sif ≈ 41`) and the server idle (`sif = 0`), *not* on doorbell
exhaustion (`sif ≤ 42 ≤ 43` doorbells) — a deeper flow-control / h2-scheduling
interaction not yet root-caused. Because of it the benchmark client still caps
read-ring gRPC to `in_flight = 1` (with a warning); see
[`run_channel_clients`](../../tests/rdma-io-bench/src/client.rs). read-ring at
`in_flight = 1` and the direct echo path (`--mode echo`) were never affected.

The same residual stall reproduces on MANA in a **unidirectional** integration
test (`pipelined_transfer_read_ring`) — a lost wakeup on the read side, not a
gRPC-only phenomenon. See
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
   the `drain_send_cq_for_read` dual-arm remains a latent hazard to clean up.

Before lifting the bench `in_flight = 1` cap, validate fix A at `in_flight`
4/8/16 on the MANA RoCEv2 preview (the CM is flaky there, so use
`reboot_between`/retries).

## MANA unit-test reproduction: `pipelined_transfer` (unidirectional stall)

The residual sustained-load stall also reproduces in the **integration tests on
MANA hardware** — it is *not* purely a gRPC/bidirectional phenomenon. Running the
suite on a MANA VM (via
[`run_unit_tests.yml`](../../tests/e2e/playbooks/run_unit_tests.yml) /
`just test-remote`), `pipelined_transfer_read_ring`
([async_stream_tests.rs](../../rdma-io-tests/tests/async_stream_tests.rs)) fails
intermittently — ~1/3 of runs. `pipelined_transfer_default` (send-recv) shows a
rarer analogous stall. **rxe/CI does not reproduce it**; it is timing-dependent
and only surfaces on the MANA preview's latencies.

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
the bench residual stall, i.e. a **lost wakeup on the read side** (the recv-CQ
notification arming vs. a completion that already arrived).

**Test-level fixes do not work** (and were reverted):
- Keeping the reader's `server` alive past the `read()` loop (return it from the
  task instead of dropping) removes only a *secondary* end-of-transfer drop race.
- Reading to EOF instead of a fixed byte count (so the reader keeps *polling* the
  transport until the writer's graceful shutdown) improved the pass rate
  (~6/15 → ~10/15) by eliminating that drop race, but ~1/3 still stalled on the
  underlying lost wakeup.

A test cannot paper over a transport lost-wakeup, and shrinking the blast would
just hide a real bug that CI is meant to catch, so the test is left unchanged and
this is tracked here. The fix belongs in the transport/`AsyncCq` notification
path (see deferred work above); fixing it would also let the benchmark lift its
read-ring `in_flight = 1` cap.

**Note on the full MANA suite.** Every other integration test passes on MANA.
Memory-Window, atomics, and siw/rxe-only tests self-skip via the `require_*`
macros ([lib.rs](../../rdma-io-tests/src/lib.rs)); the ring transports auto-detect
`max_mw = 0` and fall back to the MR rkey (`MemoryWindowMode::Auto`).
`pipelined_transfer_read_ring` is the sole remaining flake, and it is this stall.
