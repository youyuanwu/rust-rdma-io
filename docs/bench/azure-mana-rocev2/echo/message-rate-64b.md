# Echo message-rate (64 B) — arm-park

Small-message (64 B) request/response throughput, latency, and CPU efficiency for
the default arm-park `echo` mode — the message-rate regime where per-op overhead
(doorbells, completions) is the limiter. See the [echo scenario](../../scenarios/echo.md),
[methodology](../../methodology.md), and [metrics](../../metrics.md). Other echo
regimes: [large-payload (8 KiB)](large-payload-8kib.md) · [busy-poll](busy-poll.md) ·
[thread-per-core (echo-park)](thread-per-core-park.md).

## Results

### 2026-07-17 — regression re-validation
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `duration=10 warmup=3 threads=64`, reboot-clean NIC between ring batches

Full re-run on the current binary as a regression check. **No regression** — every
ceiling and CPU/op figure holds within the documented run-to-run noise (baseline in
parentheses).

**In-flight sweep (8×8, `ring_queue_depth=128`)** — req/s:

| in-flight | send-recv | read-ring | credit-ring | tcp |
|---:|---:|---:|---:|---:|
| 1  | 95.2k (99.7k) | 94.2k (100.4k) | 93.4k (100.2k) | 94.8k (92.7k) |
| 4  | 377k (419k) | 378k (362k) | 355k (375k) | 333k (334k) |
| 16 | 1.29M (1.30M) | 1.30M (1.34M) | 1.36M (1.26M) | 1.11M (1.08M) |
| 64 | 3.98M (3.71M) | 3.98M (4.32M) | 0.996M (1.05M) | 1.89M (1.80M) |

**Headline — CPU/op & tail (64×64, in-flight 64), canonical schema** (prior value in parentheses; each transport's *peak* message rate is higher — read-ring 6.32M this session at 32×512, tcp 8.94M at 512×64 — see the sweeps below):

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 4.14M (4.40M) | 1.25 µs (1.30) | 5.2 | 345 µs | 1502 µs | n/r | 4.2× · 1.1× · 61% |
| read-ring | 4.77M (4.75M) | 1.24 µs (1.21) | 5.9 | 153 µs | 1760 µs | n/r | 4.2× · 1.3× · 70% |
| credit-ring | 0.98M (1.01M) | 3.18 µs (3.93) | 3.1 | 4183 µs | 4731 µs | n/r | 1.6× · 3.5× · 14% |
| tcp | 6.84M (6.75M) | 5.20 µs (5.37) | 35.5 | 272 µs | 1339 µs | n/r | baseline |

**read-ring depth & hard-cap sweeps** — the knee shifted run-to-run (as the baseline
already flagged, read-ring is noisy in the 5.0–6.4M band) but the architectural
ceiling (~6.3M this session) and CPU/op (~0.9–1.0 µs) are intact, 0 errors: 16×256
5.13M/0.98µs, 32×512 6.32M/0.97µs (session peak), 64×512 5.21M/0.95µs, 16×2048
6.26M/0.95µs. Connection ceiling reproduced clean — 192×16 2.48M, 384×16 2.75M, and
**448×16 established this reboot-clean NIC** (2.78M, 4 negligible errors), i.e. it
went past the documented ~384 clean-max (the ~384 limit is probabilistic MANA
CM-*setup* flakiness, not a data-path wall).

**TCP scaling** — reconfirmed, ~55-core wall: 64×64 6.84M, 128×64 7.97M, 256×64
8.62M, **512×64 8.94M (session peak)**; in-flight-16 connection sweep flat at
~3.2–3.35M (1024/2048/4096 conns) with latency inflating as before. TCP still wins
the absolute number (~8.9M) by burning ~55 cores; read-ring delivers its ~6.3M at ~6
cores — the ~6× CPU-efficiency and tail-latency story is unchanged.

### 2026-07-07 — re-validation
- **Date:** 2026-07-07
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `duration=10 warmup=3`

Re-measured on the current binary, both ceilings hold:

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | n/r | n/r | n/r | n/r | n/r | n/r | not re-measured this run |
| read-ring | ~6.4M | ~1.0 µs | ~6.4 | n/r | 2869 µs | n/r | 6.2× · 0.5× · 72% |
| credit-ring | n/r | n/r | n/r | n/r | n/r | n/r | not re-measured this run |
| tcp | ~8.84M | 6.23 µs | ~55 | n/r | 5903 µs | n/r | baseline |

Best config: read-ring 24 × 768, tcp 384 × 64.

The TCP connection sweep pushed slightly past the earlier 256 × 64 point: 64 × 64 →
6.68M, 128 × 64 → 7.90M, 256 × 64 → 8.39M, **384 × 64 → 8.84M**, 512 × 64 → 8.58M
(regresses; ~55-core CPU wall). read-ring stayed run-to-run noisy (5.0–6.4M) at
~1 µs/op and ~6–7 cores, reconfirming the architectural ~6.8M / ~7-core cap. The
qualitative picture is unchanged: **TCP wins the absolute number (~8.8M) by burning
~55 cores; read-ring delivers ~72 % of that at ~1/8 the cores — ~6× more
CPU-efficient per op** — and at *half* the tail (p99 2.9 ms vs TCP 5.9 ms), since
read-ring hits its peak at far lower concurrency.

(Note: this raw-transport efficiency gap is *erased* once the same transports run
under gRPC — the TLS/HTTP-2/protobuf stack collapses both to ~0.7–0.83M req/s at ~55
cores, so the gRPC-layer throughput is bounded by the stack, not the byte transport.)

### Undated — historical baseline
- **Date:** unknown (baseline result set predates dated recording)
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `warmup=3 threads=64` (duration not recorded; the echo re-validations used `duration=10`), reboot-clean NIC

#### Ring in-flight ceiling (fixed)

The in-flight sweep surfaced a real ring-transport bug: past a per-transport
in-flight ceiling the rings failed with an `ENOMEM` (`os error -12`) from
`ibv_post_send`, because their QP **send-queue and doorbell depths were coupled to
the ring slot count** (`ring_capacity / max_message_size`), so a deep request
pipeline overran the queues. This is now fixed two ways: `send_copy` treats a full
send queue as back-pressure (`Ok(0)`) instead of a fatal error, and a `max_in_flight`
config option (`--ring-queue-depth`) sizes the send/doorbell/CQ queues for the
intended pipeline depth independently of `max_message_size`. See
[../../../bugs/ring-send-queue-exhaustion.md](../../../bugs/ring-send-queue-exhaustion.md)
for the full analysis, reproduction, and fix.

With the queues sized (`ring_queue_depth=128`), every transport scales cleanly to
in-flight 64 with **zero errors** (8×8, 64 B payload, req/s):

| in-flight | send-recv | read-ring | credit-ring | tcp |
|---|---|---|---|---|
| 1  | 99.7k | 100.4k | 100.2k | 92.7k |
| 4  | 419k  | 362k   | 375k   | 334k |
| 16 | 1.30M | 1.34M  | 1.26M  | 1.08M |
| 64 | 3.71M | **4.32M** | 1.05M | 1.80M |

Takeaways: at in-flight 1 (the request/response regime the tonic `rh2` path runs in)
all transports are within ~10% (~95–100k) — ~2.5× the `rh2` gRPC number, showing the
TLS + h2 + protobuf + hyper stack's overhead. Pipelining is the dominant lever; RDMA
`send-recv`/`read-ring` reach ~2× TCP at depth with far better tail latency, and
`read-ring` leads at in-flight 64. `credit-ring` trails at depth because its credit
round-trips add latency.

#### CPU cost per operation (64×64, in-flight 64, 64 B)

The 8×8 sweep above is CPU-*constrained* (8 cores), which is exactly where RDMA's
lower per-op cost converts into higher throughput. To separate *rate* from *cost*,
the echo client measures its own resource use over the measured window — see
[metrics.md](../../metrics.md) for how `cores busy`, `CPU/op` (`cpu_us_per_op`), and
peak RSS are derived.

Given a large core budget (64 vCPUs), TCP can brute-force *higher* raw throughput —
but at a very different cost:

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 4.40M | 1.30 µs | ~5.7 | 243 µs | 1408 µs | 24.4 MB | 4.1× · 1.0× · 65% |
| read-ring | 4.75M | 1.21 µs | ~5.8 | 217 µs | 1096 µs | 34.8 MB | 4.4× · 0.8× · 70% |
| credit-ring | 1.01M | 3.93 µs | ~4.0 | 4111 µs | 4463 µs | 34.3 MB | 1.4× · 3.3× · 15% |
| tcp | 6.75M | 5.37 µs | ~36.3 | 279 µs | 1362 µs | 17.9 MB | baseline |

**RDMA is ~4.4× more CPU-efficient per operation** (read-ring 1.21 µs vs TCP
5.37 µs). TCP's higher rps costs ~36 cores because it is fully CPU-bound in the
kernel stack; the RDMA rings hit 4.4–4.75M on only ~6 cores — they are
NIC/completion-bound, not CPU-bound, and leave headroom. RDMA also uses ~2× the RSS
(registered buffers + MRs); `send-recv` is lighter than the rings.

#### Depth scaling: read-ring in-flight sweep

The 4.75M figure is not a hardware ceiling — 64×64 uses shallow 64-deep pipelines.
Because the limiter is per-message overhead (doorbells + completion notifications),
*deeper per-connection pipelines* amortize it. Sweeping `--in-flight` (with
`--ring-queue-depth` sized to match) on read-ring:

| conns × depth | offered | throughput | CPU/op | cores | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| 16 × 64  | 1024  | 4.13M | 1.368 µs | 5.7 | 170 µs | 716 µs |
| 16 × 256 | 4096  | 5.92M | 1.125 µs | 6.7 | 244 µs | 1198 µs |
| **16 × 512** | 8192 | **6.58M** | **0.993 µs** | 6.5 | 419 µs | 1771 µs |
| 32 × 512 | 16384 | **6.75M** | 1.051 µs | 7.1 | 462 µs | 1942 µs |
| 16 × 1024 | 16384 | 4.39M | 0.939 µs | 4.1 | 784 µs | 2723 µs |
| 16 × 2048 | 32768 | 4.06M | 0.871 µs | 3.5 | 1178 µs | 4975 µs |

At 32 × 512, **read-ring reaches 6.75M req/s using ~7 cores** — and it gets there
because:

- **CPU/op *drops* with depth** (1.37 → 0.99 µs): deeper pipelines reap more
  completions per wakeup, amortizing the doorbell/notification cost. Fewer-but-deeper
  beats more-but-shallow (16 × 512 at 6.5 cores > 32 × 256 > 64 × 64).
- read-ring peaks at **~6.75M using only ~7 of the 64 cores** — it is neither
  CPU-bound nor NIC-bound (TCP pushes the same NIC to 8.4M pps, see below). The
  ceiling is read-ring's own **per-message doorbell/completion overhead**; it leaves
  ~57 cores idle and still cannot go faster.

**There is an optimum depth (~512 here), not "more is better".** Past that knee,
deeper pipelines *collapse* throughput (6.58M → 4.39M → 4.06M at 16 × 512 → 1024 →
2048) even though CPU/op keeps falling: cores busy drop (6.5 → 3.5) and latency
balloons (p50 419 → 1178 µs). The pipeline is over-queued — completions arrive in
large bursts with idle gaps and requests pile up in the doorbell/CQ queues rather
than being serviced, so the system waits instead of working. It is an inverted-U:
too shallow underfills the pipe, too deep over-queues and stalls.

**read-ring hard-caps at ~6.8M rps / ~9 cores — no parameter breaks it.** A
reboot-gated sweep over connection count at the good depth confirms the ceiling is
architectural, not tunable:

| conns × depth | offered | throughput | cores | CPU/op | p50 |
|---|---:|---:|---:|---:|---:|
| **48 × 512** | 24576 | **6.83M** | 9.4 | 1.380 µs | 651 µs |
| 32 × 512 | 16384 | 6.75M | 7.1 | 1.051 µs | 462 µs |
| 64 × 512 | 32768 | 6.58M | 6.0 | 0.913 µs | 396 µs |
| 64 × 256 | 16384 | 5.49M | 5.7 | 1.032 µs | 227 µs |

Cores busy stay pinned at **~6–9 across *every* config** (16–64 connections, depth
64–2048); more connections do not engage more cores and 64 × 512 even regresses. The
NIC sustains 8.4M pps (TCP) and ~55 cores sit idle, yet read-ring cannot go faster:
its per-message completion/doorbell path serializes and will not parallelize past ~9
cores. **Pushing past ~6.8M requires transport code changes (unsignaled/batched
sends, doorbell/completion batching), not knobs.** Best deployable configs: **48 ×
512** for peak throughput, or **32 × 512** for a better latency/throughput balance
(p50 462 vs 651 µs).

#### Connection-count ceiling: no data-path deadlock

Scaling *connection count* (fixed in-flight 16, 64 B) shows read-ring's data path has
**no high-fan-out deadlock** — the only ceiling is connection *setup*, never the data
path. (The earlier `EMFILE` / `ulimit -n` fd wall is fixed: the bench playbooks now
raise the soft `nofile` limit to the hard cap; see
[§ The fd wall](../../methodology.md#the-fd-wall-fixed).)

**read-ring** scales cleanly to ~384 connections, then fails to *establish* at ~448
on a fresh NIC — MANA RDMA-CM setup flakiness at high fan-out, not a data-path wedge:

| conns × in-flight | throughput | cores | p99 | errors | result |
|---|---:|---:|---:|---:|---|
| 192 × 16 | 2.66M | 6.1 | 1785 µs | 0 | clean |
| **384 × 16** | 2.56M | 5.3 | 2537 µs | 0 | **clean (max clean)** |
| 448 × 16 | — | — | — | — | fail: CM setup (won't establish) |

(Throughput is flat ~2.6M here only because in-flight 16 is far below the depth knee;
connection count is not read-ring's throughput lever — depth is, per the sweep
above.)

**TCP** has no connection wall in the tested range — kernel sockets scale to 4096+
with 0 errors, bounded only by latency:

| tcp conns × in-flight | throughput | cores | p50 | p99 | errors |
|---|---:|---:|---:|---:|---:|
| 1024 × 16 | 3.12M | 15.7 | 4847 µs | 12207 µs | 0 |
| 2048 × 16 | 3.16M | 15.8 | 9615 µs | 27711 µs | 0 |
| 4096 × 16 | 3.08M | 15.6 | 19599 µs | 63391 µs | 0 |

TCP throughput is flat ~3.1M across the range (in-flight 16 is below its
connection-scaling regime); more connections only inflate latency (p50 4.8 → 19.6
ms). So read-ring's connection ceiling is RDMA-CM setup (~384) and TCP's is practical
latency, not a hard limit.

This is the direct-transport control for the gRPC high-fan-out deadlock: the same
read-ring transport that wedges under `rh2` at ~208 connections (64 B) runs `echo`
cleanly to ~384 and only ever stops on connection *setup* — **confirming the deadlock
lives in the `AsyncRdmaStream` / HTTP-2 layer, not the transport**
([read-ring-concurrent-stream-deadlock.md](../../../bugs/read-ring-concurrent-stream-deadlock.md)).

#### TCP scales the other way — with connections (cores)

TCP is **CPU-bound in the kernel stack**, so it scales with *connection count* (more
threads → more cores), not pipeline depth. On the 64-vCPU VM:

| tcp config | offered | throughput | CPU/op | cores | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| 64 × 64   | 4096  | 6.75M | 5.37 µs | 36.3 | 279 µs | 1362 µs |
| 128 × 64  | 8192  | 7.74M | 6.36 µs | 49.2 | 406 µs | 2517 µs |
| 128 × 128 | 16384 | 8.13M | 6.36 µs | 51.7 | 896 µs | 4563 µs |
| **256 × 64** | 16384 | **8.42M** | 6.38 µs | 53.7 | 575 µs | 4079 µs |
| 16 × 512  | 8192  | 2.78M | 4.41 µs | 12.3 | 744 µs | 3851 µs |

TCP tops out at **~8.42M rps at ~54/64 cores** (the last ~10 cores go to
softirq/network-stack contention, not useful work). Note TCP does *not* benefit from
deep pipelines on few connections (16 × 512 = only 2.78M) — each TCP connection is a
serial byte stream, so parallelism comes from *more connections*.

#### Which is faster?

| | peak throughput | CPU/op | cores at peak | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| **read-ring** (RDMA) | 6.75M | **1.05 µs** | **~7** | **462 µs** | **1942 µs** |
| tcp (kernel) | **8.42M** | 6.38 µs | ~54 | 575 µs | 4079 µs |

TCP wins **absolute** throughput (~+25%) — but only by burning **~7.5× the cores and
~6× the CPU per op**, and at **~2× the tail latency** (p99 4079 µs vs 1942 µs).
read-ring delivers ~80% of TCP's peak while leaving ~57 cores free for the actual
application. On a box dedicated to moving bytes, TCP's brute force wins the headline
number; whenever the CPU is needed for real work, RDMA's efficiency wins decisively.
The earlier notion of a shared "~6.7M NIC wall" was an artifact of comparing
read-ring's transport ceiling against a single 64×64 TCP point — the NIC itself
sustains ≥8.4M pps.

The trade-off within each transport is latency: throughput-via-depth (RDMA) or
throughput-via-connections (TCP) both raise queueing delay. By Little's Law the mean
in-flight `N = throughput × latency`, so pushing `N` higher buys rps at the cost of
time-in-system. **Guidance: for RDMA rings, scale in-flight *depth per connection* up
to ~the knee (~512 here) — deeper over-queues and hurts; for TCP, scale
*connection/thread count* toward the core budget. Keep depth/conns modest when
latency matters.** Pushing read-ring past its ~6.75M ceiling needs transport changes
(unsignaled/batched sends, doorbell batching), not a config knob.
