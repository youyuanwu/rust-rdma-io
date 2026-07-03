# Why tonic-over-RDMA trails TCP at low concurrency

**Status:** Analysis + fix (multiple-in-flight implemented 2026-07-03)
**Date:** 2026-07-01 (updated 2026-07-03)
**Context:** MANA RoCEv2 preview, `rdma-io-bench` gRPC matrix (rh2 = tonic/HTTP-2
over RDMA, tcp = tonic-tls over kernel TCP). Both paths run TLS.

---

## Observation

At **8 connections × 8 threads**, small payloads, gRPC-over-RDMA (`rh2`) is
*slower* than the kernel-TCP baseline:

| payload | rh2 send-recv | rh2 read-ring | rh2 credit-ring | tcp baseline |
|--------:|--------------:|--------------:|----------------:|-------------:|
| 64 B    | ~40k req/s    | ~46k          | ~56k            | ~50k         |
| 8 KiB   | ~17k req/s    | ~10k          | ~17k            | —            |

So at 64 B, `rh2/send-recv` (~40k) trails TCP (~50k). This is counter-intuitive
for a kernel-bypass fabric, but expected given the workload (tiny requests, low
queue depth) and the current stream mechanics.

**Key evidence it is *not* a fundamental deficit:** the same stack scales ~4× to
**~161k req/s at 64×64**. The 8×8 result is **latency-/concurrency-bound**, not a
transport ceiling.

> **Resolved (2026-07-03).** The one-signaled-send-in-flight write gate was the
> limiter. With an N-deep send pipeline + concurrent RPCs, rh2 reaches **TCP
> parity** (send-recv within ~4 %, credit-ring within ~0.3 % at 32×32/if16). See
> the [Update](#update-2026-07-03-multiple-in-flight-implemented) section.

---

## Root causes (transport mechanics)

### 1. One signaled send in flight, awaited synchronously
[`poll_write_slice`](../../rdma-io/src/async_stream.rs) posts a single
**signaled** send (`send_copy` → `post_send_signaled`) and blocks the write until
its CQ completion arrives. `write_pending` permits only **one outstanding send**.
So every gRPC write pays a full *post → doorbell → CQE → reactor-wakeup* round
trip before it can proceed.

TCP's `poll_write` copies into the kernel socket buffer and returns `Ready`
immediately — no completion wait. That structural difference makes each RDMA
write more expensive than a TCP write.

### 2. No batching — one message per write, one completion per read
The kernel TCP stack coalesces aggressively (TSO/GSO on send, GRO on recv,
cork/Nagle), so many bytes and multiple requests ride a single syscall and NIC
descriptor. Our path is strictly **one RDMA message per write, one recv
completion per read** — a ping-pong with no pipelining. At 64 B the fixed
per-message cost dominates.

### 3. Per-message completion-notification overhead
Each completion path is: CQE → arm CQ (`ibv_req_notify_cq`) → comp-channel fd
readable → tokio `AsyncFd` reactor → wake task → re-poll. That is an epoll-class
readiness round trip **per completion, in both directions, per request** —
comparable to TCP's epoll wakeup but incurred more often, plus the "arm the CQ"
step.

### 4. Copy-based send/recv — no zero-copy win over TCP
`send_copy` memcpy's the payload into a registered MR before posting, and the
read path copies out of the recv MR into the caller's buffer. RDMA's headline
zero-copy advantage is therefore not in play; we pay a copy on both sides just
like TCP, plus the WR/CQE overhead TCP does not have.

### 5. Small messages are RDMA's worst case
Posting a WR, ringing the doorbell, DMA'ing the descriptor, and generating a CQE
is fixed cost per message. RDMA wins on large transfers and deep queues; at 64 B
it is nearly all overhead, whereas TCP amortizes it via batching.

### 6. Low queue depth cannot hide latency
With 8 connections and one in-flight send each, there are not enough concurrent
requests to keep the NIC busy while others await completions. TCP hides this
cheaply with kernel buffering. Raising concurrency (64×64) recovers ~4×.

---

## What is *not* the cause (measured, ruled out)

| Hypothesis | Verdict |
|---|---|
| Per-request **fragmentation** into many sends | Ruled out. h2's `FramedWrite` pre-coalesces frames ≤ `chain_threshold` (256 B w/ vectored, 1024 B w/o) into one contiguous write, so small requests were always one send. Adding `poll_write_vectored` measured **flat**. |
| `futures-io` → `tokio` **`Compat` read zero-fill** | Ruled out as the bottleneck. Native `tokio::io` traits (read via `ReadBuf::put_slice`, no `initialize_unfilled` memset) measured **flat** — the zeroed region is small and cheap vs. the RDMA RTT. |
| **TLS** overhead | Neutral. Both `rh2` and the `tcp` baseline run `tonic-tls`. |
| **HTTP/2** framing / HPACK | Neutral. Shared by both gRPC paths. |

The vectored-write and native-tokio changes were kept anyway — they are
correctness/cleanliness wins (native traits, proper vectored support, one fewer
shim + dependency), just perf-neutral for this workload. See
[TonicIntegration.md](Internal/TonicIntegration.md).

---

## Highest-leverage fixes

Ordered by expected impact on the low-concurrency small-message case:

1. **Multiple sends in flight with selective signaling** — signal every Nth WR
   instead of one-signaled-send-at-a-time; removes the per-write CQ round trip.
   See *Double-Buffered Sends* in
   [StreamImprovements.md](../future/StreamImprovements.md#2-double-buffered-sends)
   (generalize the 2-slot design to N slots). **These are two separable changes
   with very different portability — see [below](#transport-applicability-of-fix-1).**
   **✅ Multiple-in-flight is implemented (2026-07-03) — measured 2.7–3.4×; see
   the [Update](#update-2026-07-03-multiple-in-flight-implemented) section below.
   Selective signaling is still open.**
2. **Inline data for small messages** (`max_inline_data = 64`) — eliminates a
   PCIe DMA-read per small send. See
   [StreamImprovements.md](../future/StreamImprovements.md#1-enable-inline-data-for-small-messages).
3. **Optional busy-poll before arming the CQ** — skips the arm/block cycle when
   completions are already present under load. See
   [StreamImprovements.md](../future/StreamImprovements.md#3-configurable-busy-poll).
4. **Deeper benchmark queue depth** — more connections/streams to hide the
   residual per-request latency (already validated: ~161k at 64×64).

### Transport applicability of fix #1

"Multiple sends in flight" and "selective signaling" are **separable**, and only
the first is a single uniform change.

**Multiple sends in flight — works for all three transports (a stream-level change).**
The transports already accept back-to-back sends before prior ones complete:

- **send-recv** — [`send_copy`](../../rdma-io/src/send_recv_transport.rs) round-robins
  over `num_send_bufs` buffers with per-buffer `send_in_flight[idx]` tracking and
  sizes the send CQ as `num_send_bufs + 1`. It is already multi-buffer capable;
  `num_send_bufs = 1` (default) is the only reason it is one-at-a-time.
- **credit-ring / read-ring** — `send_in_flight` is a *counter*; back-pressure is
  gated by `remote_credits` / `remote_free_space`, not a 1-in-flight cap.

The real limiter is [`AsyncRdmaStream`](../../rdma-io/src/async_stream.rs)'s
`write_pending: Option<usize>` (single slot). Widening it to N slots lifts the cap
for **all three** uniformly, since it only relies on `send_copy` returning `Ok(n)`
and accepting another call.

**Selective (deferred) signaling — feasible on all three, but three separate impls.**
It is safe on every path because all three ride an **RC QP** (strict in-order
completion, so a periodic signaled CQE certifies all preceding unsignaled WRs are
done). But each transport does real work *on every completion today*, so CQEs
cannot simply be dropped — the work must be deferred to the signaled one, and the
accounting differs:

| Transport | Per-completion work today | Rework for selective signaling |
|---|---|---|
| **send-recv** | Frees one buffer (`wr_id → buf_idx`) | Easiest: on the signaled CQE, free every buffer posted since the last signal. |
| **credit-ring** | Reclaims send-ring bytes (`wr_id` encodes `padding + data` release length), adjusts `send_in_flight`/credits; padding WRs also consume a credit + CQE | Accumulate release lengths in a FIFO and reclaim on the signaled CQE; fold in padding WRs. Deferring reclamation also shrinks ring headroom → hits ring-full sooner. |
| **read-ring** | Same ring reclaim, **plus** the send CQ also carries RDMA **Read** completions (offset refresh, `read_in_flight`) | Same ring-reclaim rework, and mixing signaled reads with unsignaled writes on one CQ needs care. |

Common caveats for all three: the SQ has a fixed `max_send_wr`, so you must signal
often enough to reap completions and free SQ slots, and `poll_flush` / `poll_close`
must drain the final unsignaled batch before returning / sending DREQ.

**Bottom line:** multiple-in-flight is essentially universal (one stream-level
change); selective signaling is achievable on all three but is trivial for
send-recv, a release-accounting rework for credit-ring, and additionally a
mixed-CQ concern for read-ring.

---

## Update (2026-07-03): multiple-in-flight implemented

Fix #1 (multiple sends in flight) — and the client-side concurrency needed to
exercise it — are now implemented and measured. Two changes.

### 1. Stream layer: post-and-return, N-deep pipeline

[`AsyncRdmaStream::poll_write_slice`](../../rdma-io/src/async_stream.rs) no longer
posts one signaled send and blocks on its completion. It now **posts and returns
immediately**, letting consecutive writes keep up to the transport's send-buffer
count outstanding at once. The single-slot `write_pending: Option<usize>` gate is
replaced by a `write_blocked: bool` (scratch-preservation only).

Pattern / invariants:

- **Backpressure comes from the transport, not a stream-level counter.**
  `send_copy` returns `Ok(0)` when every send buffer is in flight (send-recv) or
  credits are exhausted (rings); only then does the stream reap a completion and
  retry. On a successful post it returns `Ok(n)` *without* waiting — matching
  TCP's "accepted into the buffer, not yet ACKed" semantics (the payload is
  already copied into the registered MR, so the caller's buffer is free to reuse).
- **`poll_flush` stays a no-op** — a posted send is already on the wire. Only
  `poll_close` must drain: it waits for the new `Transport::sends_in_flight()` to
  reach 0 before `disconnect()`, so pipelined sends are delivered rather than
  flushed by the QP's transition to ERROR.
- **Sizing:** `SendRecvConfig::stream_with_depth(depth)` sizes `num_send_bufs`
  (and matching recv buffers) so the QP can actually hold `depth` sends; both
  peers must be sized for the depth they intend to pipeline. `stream()` (depth 1)
  is unchanged. The ring transports already allow many in-flight via credits.
- **Gotcha (bit us during bring-up).** The blocked-write path drains the recv CQ
  to catch ring credit updates, and that poll is *destructive*. For send-recv
  those completions are real **responses**, so they must be stashed
  (`recv_stash`) and served by `poll_read` before it polls the transport —
  otherwise responses are dropped and the client hangs (every run timed out until
  the stash was added).

### 2. Benchmark: `--in-flight` drives concurrent RPCs per connection

A deep transport pipeline only fills if the application keeps multiple requests
outstanding. The gRPC client's `--in-flight N`
([client.rs](../../tests/rdma-io-bench/src/client.rs)) now spawns `N` concurrent
request loops per connection (each an independent `say_hello` loop sharing the
connection's `Channel`); h2 multiplexes them as concurrent streams over the one
RDMA connection. The server takes a matching `--in-flight` to size its stream
depth. (Previously `--in-flight` was echo-mode only and the gRPC path was
hard-wired to one outstanding RPC per connection.)

### Measured (rh2/send-recv, 64 B, MANA RoCEv2, 0 errors everywhere)

**32×32 depth sweep:**

| in_flight | rps | p50 µs | p99 µs | vs if=1 |
|----:|----:|----:|----:|----:|
| 1 | 159,786 | 190 | 350 | 1.00× |
| 2 | 249,978 | 249 | 413 | 1.56× |
| 4 | 331,149 | 378 | 658 | 2.07× |
| 8 | 327,919 | 533 | 1,032 | 2.05× |
| 16 | 415,286 | 1,118 | 2,177 | 2.60× |
| 32 | 452,955 | 2,151 | 4,183 | 2.83× |

**64×64:**

| in_flight | rps | p50 µs | p99 µs | vs if=1 |
|----:|----:|----:|----:|----:|
| 1 | 193,795 | 312 | 642 | 1.00× |
| 8 | 531,684 | 619 | 1,175 | 2.74× |

At 64×64 the stream layer went from the original one-in-flight **~156k** (top of
this doc) to **531.7k** — **3.4×** — with zero errors.

### The pattern: depth trades latency for throughput (Little's Law)

Throughput rises monotonically with depth while p50 grows roughly linearly,
exactly as `rps ≈ concurrency / latency` predicts. Depth is a **tuning knob**,
not a free win:

- **if=2** — cheapest win: +56 % rps for +30 % p50.
- **if=4** — the knee: ~2× rps at ~2× p50 (378 µs). Good default for a
  latency-sensitive service.
- **if=16–32** — throughput mode: up to ~2.8×, but p50 balloons 6–11×.

This is the same pipelining lesson the raw-transport echo benchmark showed
([EchoBenchmark.md](EchoBenchmark.md) §8), now applied one layer up through
gRPC/HTTP-2. Reproduce with e.g.
`just run-bench mode=rh2 transport=send-recv connections=32 threads=32 in_flight=8`.

### At tuned depth, gRPC-over-RDMA reaches TCP parity

The gap this doc opens with — rh2 *trailing* the TCP baseline — closes once both
run at the same application concurrency. Matrix at **32×32, in_flight=16, 64 B,
0 errors** (`just bench-matrix "rh2 tcp" "send-recv read-ring credit-ring" 32 10 64 true 0 true 16`):

| transport | rps | p50 µs | p99 µs |
|---|---:|---:|---:|
| rh2 / send-recv | 411,552 | 1,131 | 2,195 |
| rh2 / credit-ring | 425,905 | 1,168 | 2,303 |
| rh2 / read-ring | — (CM wedge, unmeasured) | | |
| tcp baseline | 427,034 | 1,148 | 2,355 |

send-recv lands within ~4 % of TCP and credit-ring within ~0.3 %, with p50/p99
essentially identical (RDMA's p50 is marginally lower). Contrast the depth-1
numbers at the top of this doc (8×8: ~40k vs ~50k; 64×64: 156k vs 227k, ~30 %
behind).

Two honest caveats:

- **TCP scales with the same knob.** `--in-flight` is concurrent RPCs per
  connection, which helps *both* stacks; TCP rose from ~227k (depth 1) to 427k
  here. So this is RDMA **catching up to** TCP on raw gRPC throughput, not beating
  it. RDMA's standing win is **CPU efficiency per op** (see the raw-transport echo
  results in [EchoBenchmark.md](EchoBenchmark.md) §8), not req/s.
- **read-ring is capped at `in_flight = 1`** over gRPC — it has a known
  concurrent-stream deadlock (bidirectional send-queue-full / doorbell RNR stall;
  see [read-ring-concurrent-stream-deadlock.md](../bugs/read-ring-concurrent-stream-deadlock.md)),
  so the benchmark client forces its depth to 1 until that transport fix lands.
  send-recv and credit-ring are the pipelined RDMA gRPC transports on this NIC.

---

## Ring transports at larger payloads (and a latent sizing bug)

### Why read-ring is slower than credit-ring at 8 KiB

At 64 B all transports are close (read-ring is actually *fastest*, ~47k); at
8 KiB read-ring drops to ~10k while credit-ring and send-recv hold ~17k. Two
factors:

1. **The bench builds both rings with `::datagram()`** ([client.rs](../../tests/rdma-io-bench/src/client.rs)),
   which sets `max_message_size = 1500`. The ring `send_copy` clamps every send
   to 1500 B, so an 8 KiB payload becomes ~6 RDMA messages. send-recv uses
   `::stream()` (64 KiB buffer) → one send. This hits *both* rings equally.
2. **Flow control differs — pull vs push:**
   - **credit-ring** is **push**-based: the receiver proactively sends credit
     updates (Send+Imm on the recv CQ) as it drains. No extra round trip.
   - **read-ring** is **pull**-based: when free space drops below
     `data_len*2 + min_free_threshold`, `send_copy` posts an RDMA **Read** to
     refresh the remote head — a full RTT. At 8 KiB the ~6× churn keeps free
     space low, so reads fire often and inject RTT bubbles on the 1-in-flight
     send path.

### Can we raise the ring size for better perf? No — both knobs are blocked

- **`ring_capacity` is hard-capped at 65536.** Both rings pack the ring offset
  into the high 16 bits of the RDMA-Write immediate (`imm = (offset << 16) | len`,
  `len ≤ 0xFFFF`). A ring larger than 64 KiB overflows the offset field. Growing
  it needs a wider wire encoding (e.g. an in-band header), not a tunable.
- **Raising `max_message_size` within that cap breaks the rings.** Measured
  (8×8, 8 KiB, `max_message_size 1500 → 16384`, `ring_capacity` unchanged):

  | combo | result |
  |---|---|
  | send-recv (control, unchanged) | 17350 req/s, 0 err ✅ |
  | read-ring | **hang** — client 53 s timeout (deadlock) ❌ |
  | credit-ring | **1.5 req/s, 201 errors**, p50 657 ms ❌ |

  The unchanged send-recv path stayed healthy on the same NIC, so this is the
  config, not MANA flakiness.

### Is this a bug in the ring code? Yes — an unenforced invariant

The rings require **`max_message_size ≪ ring_capacity`** but neither validate nor
handle the violation:

- **credit-ring** derives `max_outstanding = ring_capacity / max_message_size`
  ([credit_ring_transport.rs](../../rdma-io/src/credit_ring_transport.rs)), and
  that single number sizes **both** the sender's credits **and** the count of
  posted doorbell recv WRs (`doorbell_bufs`). At 16 KiB it collapses to **4** —
  too few for the TLS + HTTP/2 handshake burst, so Write+Imm messages arrive with
  no recv WR to consume the immediate → RNR / errors / stall (the 201 errors).
- **read-ring** deadlocks: at 16 KiB the low-space trigger
  (`remaining < data_len*2 + threshold` ≈ 32 KiB) fires almost immediately and
  the offset-Read + reserve/wrap interplay wedges (the hang).

It is a **latent** bug — masked in practice because every shipping caller uses
`max_message_size = 1500` (a 43:1 ratio to the 64 KiB ring). But the config
exposes `max_message_size` as a public knob and a legal-looking value silently
hangs or errors instead of working or being rejected.

**Suggested fixes** (either or both):

1. **Validate at construction** — reject `max_message_size` above
   `ring_capacity / N` (for a credit floor `N`, e.g. 8) with a clear error, so a
   bad combo fails fast instead of hanging.
2. **Decouple credits/doorbells from message size** — post a fixed minimum of
   doorbell recv WRs / initial credits independent of
   `ring_capacity / max_message_size`, so large messages don't starve the
   handshake pipeline; and fix the read-ring offset-Read/reserve deadlock for
   large messages.

Until then, making large payloads fast on the rings is real transport work, not
a config tweak.

> **Update:** a related sizing coupling — the send queue / doorbells being
> derived from `ring_capacity / max_message_size` — was fixed for the
> *small-message, deep-pipeline* direction via the `max_in_flight` config option
> (fix 2 above, partially). See
> [../bugs/ring-send-queue-exhaustion.md](../bugs/ring-send-queue-exhaustion.md).
> The *large-message* breakage described here (raising `max_message_size`) is
> still open.

---

## References

- [async_stream.rs](../../rdma-io/src/async_stream.rs) — send state machine (`poll_write_slice`, N-deep post-and-return pipeline; drains `sends_in_flight()` on close)
- [send_recv_transport.rs](../../rdma-io/src/send_recv_transport.rs) — `send_copy` (signaled send, MR copy)
- [credit_ring_transport.rs](../../rdma-io/src/credit_ring_transport.rs) — push-credit flow control, `max_outstanding = ring_capacity / max_message_size`
- [read_ring_transport.rs](../../rdma-io/src/read_ring_transport.rs) — pull flow control (RDMA-Read offset refresh)
- [StreamImprovements.md](../future/StreamImprovements.md) — proposed transport-level fixes
- [TonicIntegration.md](Internal/TonicIntegration.md) — tonic data path (native tokio traits, vectored writes)

