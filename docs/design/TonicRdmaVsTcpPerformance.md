# Why tonic-over-RDMA trails TCP at low concurrency

**Status:** Analysis
**Date:** 2026-07-01
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

- [async_stream.rs](../../rdma-io/src/async_stream.rs) — send state machine (`poll_write_slice`, one-in-flight `write_pending`)
- [send_recv_transport.rs](../../rdma-io/src/send_recv_transport.rs) — `send_copy` (signaled send, MR copy)
- [credit_ring_transport.rs](../../rdma-io/src/credit_ring_transport.rs) — push-credit flow control, `max_outstanding = ring_capacity / max_message_size`
- [read_ring_transport.rs](../../rdma-io/src/read_ring_transport.rs) — pull flow control (RDMA-Read offset refresh)
- [StreamImprovements.md](../future/StreamImprovements.md) — proposed transport-level fixes
- [TonicIntegration.md](Internal/TonicIntegration.md) — tonic data path (native tokio traits, vectored writes)

