# Direct-transport echo benchmark results

Measured findings for the raw-transport `--mode echo` benchmark — a one-message
request/response echo with no gRPC / TLS / HTTP-2, isolating the transport itself.
For the echo benchmark design (stack, pipeline, poll loops, metrics) and how to
run it, see [../design/EchoBenchmark.md](../design/EchoBenchmark.md); for the gRPC
numbers see [grpc.md](grpc.md).

## Ring in-flight ceiling (fixed)

The in-flight sweep surfaced a real ring-transport bug: past a per-transport
in-flight ceiling the rings failed with an `ENOMEM` (`os error -12`) from
`ibv_post_send`, because their QP **send-queue and doorbell depths were coupled
to the ring slot count** (`ring_capacity / max_message_size`), so a deep request
pipeline overran the queues. This is now fixed two ways: `send_copy` treats a
full send queue as back-pressure (`Ok(0)`) instead of a fatal error, and a
`max_in_flight` config option (`--ring-queue-depth`) sizes the send/doorbell/CQ
queues for the intended pipeline depth independently of `max_message_size`. See
[../bugs/ring-send-queue-exhaustion.md](../bugs/ring-send-queue-exhaustion.md)
for the full analysis, reproduction, and fix.

With the queues sized (`ring_queue_depth=128`), every transport scales cleanly
to in-flight 64 with **zero errors** (8×8, 64 B payload, req/s):

| in-flight | send-recv | read-ring | credit-ring | tcp |
|---|---|---|---|---|
| 1  | 99.7k | 100.4k | 100.2k | 92.7k |
| 4  | 419k  | 362k   | 375k   | 334k |
| 16 | 1.30M | 1.34M  | 1.26M  | 1.08M |
| 64 | 3.71M | **4.32M** | 1.05M | 1.80M |

Takeaways: at in-flight 1 (the request/response regime the tonic `rh2` path runs
in) all transports are within ~10% (~95–100k) — ~2.5× the `rh2` gRPC number,
showing the TLS + h2 + protobuf + hyper stack's overhead. Pipelining is the
dominant lever; RDMA `send-recv`/`read-ring` reach ~2× TCP at depth with far
better tail latency, and `read-ring` leads at in-flight 64. `credit-ring` trails
at depth because its credit round-trips add latency.

## CPU efficiency and depth scaling

The 8×8 sweep above is CPU-*constrained* (8 cores), which is exactly where
RDMA's lower per-op cost converts into higher throughput. To separate *rate*
from *cost*, the echo client also measures its own resource use: a sampler task
snapshots `/proc/self/stat` (user+system CPU, summed across all threads) at the
warm-up and benchmark deadlines — so the delta covers **only the measured
window**, excluding connection setup and warm-up — plus the `VmHWM` peak RSS.
These surface in the result JSON as `cpu_seconds`, `cpu_us_per_op`, and
`peak_rss_kb`, and as `CPU/op µs` / `Peak RSS MB` columns in `bench-report`.

> Why in-process, not `/usr/bin/time`? Whole-process CPU is dominated by the
> long ring connection setup (~60–80 s at 64 connections), which would swamp the
> ~10 s measured window. Sampling across the window is the accurate per-op cost.

**How the metrics are derived** (all from the same client-side `cpu_seconds` =
user + kernel CPU-time consumed by every thread during the window):

- **`cores busy`** (the tables below) = `cpu_seconds / duration_secs`. It is the
  *average* number of fully-saturated cores the client used — e.g. 94 CPU-seconds
  over a 10 s window = 9.4 cores. It is a time-average (not a peak) of
  compute-equivalent cores spread by the scheduler across the 64 vCPUs, not
  pinned cores. The ceiling is 64. It counts **user + kernel** time, so TCP's
  figure is inflated by in-kernel `stime` (syscalls, softirq, TCP/IP stack)
  while RDMA is almost all user-space `utime` (kernel-bypass).
- **`CPU/op µs`** (`cpu_us_per_op`) = `cpu_seconds / total_requests × 1e6` — the
  same CPU-time divided by work done instead of by wall-time.
- Both are **client-side only**; the server does comparable echo work but is not
  sampled.

### CPU cost per operation (64×64, in-flight 64, 64 B)

Given a large core budget (64 vCPUs), TCP can brute-force *higher* raw
throughput — but at a very different cost:

| transport | throughput | **CPU/op** | cores busy | peak RSS | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| **read-ring** (RDMA) | 4.75M | **1.21 µs** | ~5.8 | 34.8 MB | 217 µs | 1096 µs |
| **send-recv** (RDMA) | 4.40M | 1.30 µs | ~5.7 | 24.4 MB | 243 µs | 1408 µs |
| tcp (kernel) | 6.75M | 5.37 µs | ~36.3 | 17.9 MB | 279 µs | 1362 µs |
| credit-ring (RDMA) | 1.01M | 3.93 µs | ~4.0 | 34.3 MB | 4111 µs | 4463 µs |

**RDMA is ~4.4× more CPU-efficient per operation** (read-ring 1.21 µs vs TCP
5.37 µs). TCP's higher rps costs ~36 cores because it is fully CPU-bound in the
kernel stack; the RDMA rings hit 4.4–4.75M on only ~6 cores — they are
NIC/completion-bound, not CPU-bound, and leave headroom. RDMA also uses ~2× the
RSS (registered buffers + MRs); `send-recv` is lighter than the rings.

### Depth scaling: read-ring in-flight sweep

The 4.75M figure is not a hardware ceiling — 64×64 uses shallow 64-deep
pipelines. Because the limiter is per-message overhead (doorbells + completion
notifications), *deeper per-connection pipelines* amortize it. Sweeping
`--in-flight` (with `--ring-queue-depth` sized to match) on read-ring:

| conns × depth | offered | throughput | CPU/op | cores | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| 16 × 64  | 1024  | 4.13M | 1.368 µs | 5.7 | 170 µs | 716 µs |
| 16 × 256 | 4096  | 5.92M | 1.125 µs | 6.7 | 244 µs | 1198 µs |
| **16 × 512** | 8192 | **6.58M** | **0.993 µs** | 6.5 | 419 µs | 1771 µs |
| 32 × 512 | 16384 | **6.75M** | 1.051 µs | 7.1 | 462 µs | 1942 µs |
| 16 × 1024 | 16384 | 4.39M | 0.939 µs | 4.1 | 784 µs | 2723 µs |
| 16 × 2048 | 32768 | 4.06M | 0.871 µs | 3.5 | 1178 µs | 4975 µs |

At 32 × 512, **read-ring reaches 6.75M req/s using ~7 cores** — and it gets
there because:

- **CPU/op *drops* with depth** (1.37 → 0.99 µs): deeper pipelines reap more
  completions per wakeup, amortizing the doorbell/notification cost. Fewer-but-
  deeper beats more-but-shallow (16 × 512 at 6.5 cores > 32 × 256 > 64 × 64).
- read-ring peaks at **~6.75M using only ~7 of the 64 cores** — it is neither
  CPU-bound nor NIC-bound (TCP pushes the same NIC to 8.4M pps, see below). The
  ceiling is read-ring's own **per-message doorbell/completion overhead**; it
  leaves ~57 cores idle and still cannot go faster.

**There is an optimum depth (~512 here), not "more is better".** Past that knee,
deeper pipelines *collapse* throughput (6.58M → 4.39M → 4.06M at 16 × 512
→ 1024 → 2048) even though CPU/op keeps falling: cores busy drop (6.5 → 3.5) and
latency balloons (p50 419 → 1178 µs). The pipeline is over-queued — completions
arrive in large bursts with idle gaps and requests pile up in the doorbell/CQ
queues rather than being serviced, so the system waits instead of working. It is
an inverted-U: too shallow underfills the pipe, too deep over-queues and stalls.

**read-ring hard-caps at ~6.8M rps / ~9 cores — no parameter breaks it.** A
reboot-gated sweep over connection count at the good depth confirms the ceiling
is architectural, not tunable:

| conns × depth | offered | throughput | cores | CPU/op | p50 |
|---|---:|---:|---:|---:|---:|
| **48 × 512** | 24576 | **6.83M** | 9.4 | 1.380 µs | 651 µs |
| 32 × 512 | 16384 | 6.75M | 7.1 | 1.051 µs | 462 µs |
| 64 × 512 | 32768 | 6.58M | 6.0 | 0.913 µs | 396 µs |
| 64 × 256 | 16384 | 5.49M | 5.7 | 1.032 µs | 227 µs |

Cores busy stay pinned at **~6–9 across *every* config** (16–64 connections,
depth 64–2048); more connections do not engage more cores and 64 × 512 even
regresses. The NIC sustains 8.4M pps (TCP) and ~55 cores sit idle, yet read-ring
cannot go faster: its per-message completion/doorbell path serializes and will
not parallelize past ~9 cores. **Pushing past ~6.8M requires transport code
changes (unsignaled/batched sends, doorbell/completion batching), not knobs.**
Best deployable configs: **48 × 512** for peak throughput, or **32 × 512** for a
better latency/throughput balance (p50 462 vs 651 µs).

### Connection-count ceiling: no data-path deadlock

Scaling *connection count* (fixed in-flight 16, 64 B) shows read-ring's data path
has **no high-fan-out deadlock** — the only ceiling is connection *setup*, never
the data path. (The earlier `EMFILE` / `ulimit -n` fd wall is fixed: the bench
playbooks now raise the soft `nofile` limit to the hard cap — `setup_rdma_hw.yml`
`limits.d` drop-in + `bench_run.yml` `ulimit -n` at launch; see
[h1.md § The fd wall](h1.md#the-fd-wall-fixed).)

**read-ring** scales cleanly to ~384 connections, then fails to *establish* at
~448 on a fresh NIC — MANA RDMA-CM setup flakiness at high fan-out, not a
data-path wedge:

| conns × in-flight | throughput | cores | p99 | errors | result |
|---|---:|---:|---:|---:|---|
| 192 × 16 | 2.66M | 6.1 | 1785 µs | 0 | clean |
| **384 × 16** | 2.56M | 5.3 | 2537 µs | 0 | **clean (max clean)** |
| 448 × 16 | — | — | — | — | fail: CM setup (won't establish) |

(Throughput is flat ~2.6M here only because in-flight 16 is far below the depth
knee; connection count is not read-ring's throughput lever — depth is, per the
sweep above.)

**TCP** has no connection wall in the tested range — kernel sockets scale to
4096+ with 0 errors, bounded only by latency:

| tcp conns × in-flight | throughput | cores | p50 | p99 | errors |
|---|---:|---:|---:|---:|---:|
| 1024 × 16 | 3.12M | 15.7 | 4847 µs | 12207 µs | 0 |
| 2048 × 16 | 3.16M | 15.8 | 9615 µs | 27711 µs | 0 |
| 4096 × 16 | 3.08M | 15.6 | 19599 µs | 63391 µs | 0 |

TCP throughput is flat ~3.1M across the range (in-flight 16 is below its
connection-scaling regime); more connections only inflate latency (p50
4.8 → 19.6 ms). So read-ring's connection ceiling is RDMA-CM setup (~384) and
TCP's is practical latency, not a hard limit.

This is the direct-transport control for the gRPC high-fan-out deadlock: the same
read-ring transport that wedges under `rh2` at ~208 connections (64 B) runs
`echo` cleanly to ~384 and only ever stops on connection *setup* — **confirming
the deadlock lives in the `AsyncRdmaStream` / HTTP-2 layer, not the transport**
([read-ring-concurrent-stream-deadlock.md](../bugs/read-ring-concurrent-stream-deadlock.md)).

### TCP scales the other way — with connections (cores)

TCP is **CPU-bound in the kernel stack**, so it scales with *connection count*
(more threads → more cores), not pipeline depth. On the 64-vCPU VM:

| tcp config | offered | throughput | CPU/op | cores | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| 64 × 64   | 4096  | 6.75M | 5.37 µs | 36.3 | 279 µs | 1362 µs |
| 128 × 64  | 8192  | 7.74M | 6.36 µs | 49.2 | 406 µs | 2517 µs |
| 128 × 128 | 16384 | 8.13M | 6.36 µs | 51.7 | 896 µs | 4563 µs |
| **256 × 64** | 16384 | **8.42M** | 6.38 µs | 53.7 | 575 µs | 4079 µs |
| 16 × 512  | 8192  | 2.78M | 4.41 µs | 12.3 | 744 µs | 3851 µs |

TCP tops out at **~8.42M rps at ~54/64 cores** (the last ~10 cores go to
softirq/network-stack contention, not useful work). Note TCP does *not* benefit
from deep pipelines on few connections (16 × 512 = only 2.78M) — each TCP
connection is a serial byte stream, so parallelism comes from *more connections*.

### Which is faster?

| | peak throughput | CPU/op | cores at peak | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| **read-ring** (RDMA) | 6.75M | **1.05 µs** | **~7** | **462 µs** | **1942 µs** |
| tcp (kernel) | **8.42M** | 6.38 µs | ~54 | 575 µs | 4079 µs |

TCP wins **absolute** throughput (~+25%) — but only by burning **~7.5× the cores
and ~6× the CPU per op**, and at **~2× the tail latency** (p99 4079 µs vs
1942 µs). read-ring delivers ~80% of TCP's peak while leaving
~57 cores free for the actual application. On a box dedicated to moving bytes,
TCP's brute force wins the headline number; whenever the CPU is needed for real
work, RDMA's efficiency wins decisively. The earlier notion of a shared "~6.7M
NIC wall" was an artifact of comparing read-ring's transport ceiling against a
single 64×64 TCP point — the NIC itself sustains ≥8.4M pps.

The trade-off within each transport is latency: throughput-via-depth (RDMA) or
throughput-via-connections (TCP) both raise queueing delay. By Little's Law the
mean in-flight `N = throughput × latency`, so pushing `N` higher buys rps at the
cost of time-in-system. **Guidance: for RDMA rings, scale in-flight *depth per
connection* up to ~the knee (~512 here) — deeper over-queues and hurts; for TCP,
scale *connection/thread count* toward the core budget. Keep depth/conns modest
when latency matters.** Pushing read-ring past its ~6.75M ceiling needs transport
changes (unsignaled/batched sends, doorbell batching), not a config knob.

### Re-validation (2026-07-07)

Re-measured on the current binary (64 B, `duration=10 warmup=3`), both ceilings
hold:

| transport | peak throughput | CPU/op | ~cores | p99 | best config |
|---|---:|---:|---:|---:|---|
| tcp (kernel) | **~8.84M** | 6.23 µs | ~55 | 5903 µs | 384 × 64 |
| read-ring (RDMA) | ~6.4M | **~1.0 µs** | ~6.4 | **2869 µs** | 24 × 768 |

The TCP connection sweep pushed slightly past the earlier 256 × 64 point:
64 × 64 → 6.68M, 128 × 64 → 7.90M, 256 × 64 → 8.39M, **384 × 64 → 8.84M**,
512 × 64 → 8.58M (regresses; ~55-core CPU wall). read-ring stayed run-to-run
noisy (5.0–6.4M) at ~1 µs/op and ~6–7 cores, reconfirming the architectural
~6.8M / ~7-core cap. The qualitative picture is unchanged: **TCP wins the
absolute number (~8.8M) by burning ~55 cores; read-ring delivers ~72 % of that at
~1/8 the cores — ~6× more CPU-efficient per op** — and at *half* the tail (p99
2.9 ms vs TCP 5.9 ms), since read-ring hits its peak at far lower concurrency.

(Note: this raw-transport efficiency gap is *erased* once the same transports run
under gRPC — the TLS/HTTP-2/protobuf stack collapses both to ~0.7–0.83M req/s at
~55 cores, so the gRPC-layer throughput is bounded by the stack, not the byte
transport.)

## Large-payload (8 KiB) ceiling: read-ring vs TCP

Everything above is 64 B — the **message-rate** regime, where per-op overhead
(doorbells, completions, syscalls) is the limiter and TCP brute-forces the
headline number with ~55 cores. At a large payload the limiter flips to
**bandwidth**, and the result inverts. Measured at `payload=8192`
(`ring_max_msg=8192` so the ring carries the full 8 KiB, `duration=10 warmup=3`,
reboot-clean NIC), tuning both connection count *and* in-flight for peak
throughput:

The **best config** column is `connections × in-flight` — the number of parallel
connections and the per-connection pipeline depth that gave that row's peak
throughput (`threads=64` for all rows). read-ring lists two rows: `24 × 8` (the
best throughput/latency balance) and `24 × 16` (its absolute peak throughput).

| transport | best config | throughput | **bandwidth** | p50 | p99 | CPU/op | ~cores |
|---|---|---:|---:|---:|---:|---:|---:|
| **read-ring** (RDMA) | 24 × 8  | 539k | **35.3 Gbps** | 259 µs | 607 µs | **5.0 µs** | ~2.7 |
| read-ring (peak)     | 24 × 16 | **549k** | **36.0 Gbps** | 408 µs | 922 µs | 5.4 µs | ~3.0 |
| **tcp** (kernel)     | 32 × 4  | 454k | 29.8 Gbps | 260 µs | 439 µs | 8.8 µs | ~4.0 |

Bandwidth is one-directional (request bytes); echo is request/response, so the
wire carries ~2× — read-ring drives **~72 Gbps aggregate**. At 8 KiB **read-ring
wins**: ~36 vs ~30 Gbps (**+20 %**) on *fewer* cores (~3 vs ~4) and ~1.8× lower
CPU/op. RDMA's one-sided zero-copy Writes move bytes through the NIC with less
overhead than TCP's kernel copy + stack, and read-ring's per-message
doorbell/completion cost — the very thing that capped it at 64 B — is amortized
across the 8 KiB payload, so it is no longer the bottleneck.

Both are **bandwidth-bound, not thread- or core-bound**: setting `threads=64`
(full vCPU count) changed nothing vs `threads=conns`, and each uses only ~3–4 of
the 64 cores. Both also have a clear in-flight knee, past which throughput stops
rising and latency inflates:

**tcp** — flat at the bandwidth wall across the whole sweep; concurrency only
buys latency, and `in_flight=1` underfills the bandwidth-delay product:

| tcp config | throughput | bandwidth | p50 | p99 |
|---|---:|---:|---:|---:|
| 32 × 1  | 226k | 14.8 Gbps | 134 µs | 245 µs |
| **32 × 4** | **454k** | **29.8 Gbps** | 260 µs | 439 µs |
| 32 × 8  | 446k | 29.3 Gbps | 493 µs | 1275 µs |
| 64 × 8  | 445k | 29.2 Gbps | 991 µs | 2841 µs |
| 16→128 × 16 | ~446k | ~29.3 Gbps | 457→4127 µs | 1116→12767 µs |

**read-ring** — rises to a knee at ~24 connections × in-flight 8–16, then
*regresses* at 32 connections (the over-queuing collapse: the byte-ring/RDMA-Read
head-refresh bubbles start to dominate):

| read-ring config | throughput | bandwidth | p50 | p99 |
|---|---:|---:|---:|---:|
| 8 × 8   | 364k | 23.8 Gbps | 124 µs | 267 µs |
| 16 × 16 | 514k | 33.7 Gbps | 293 µs | 697 µs |
| **24 × 8**  | 539k | **35.3 Gbps** | 259 µs | 607 µs |
| **24 × 16** | **549k** | **36.0 Gbps** | 408 µs | 922 µs |
| 32 × 16 | 412k | 27.0 Gbps | 1211 µs | 1672 µs |

**Guidance flips with payload size.** For small messages (message-rate bound),
TCP wins absolute throughput by spending cores while RDMA wins CPU-efficiency and
tail latency; for large messages (bandwidth bound), read-ring wins on *all* axes
— throughput, bandwidth, CPU, and cores — because zero-copy RDMA moves bulk bytes
more efficiently than the kernel TCP stack. Keep read-ring at ~24 connections and
in-flight ≤ 16 for 8 KiB; deeper or wider over-queues and hurts. (Note the ring
holds `ring_capacity / max_message_size ≈ 65536/8192 = 8` message slots at this
size, so very deep pipelines back-pressure on the byte ring — do not push
`max_message_size` toward the ring capacity.)

## Busy-poll echo ceiling (`mode=echo-busy`, read-ring)

Everything above is the **arm-park** path — completions delivered by
per-connection interrupt-armed CQs on the tokio worker pool, which spreads work
opportunistically across all 64 vCPUs. The **busy-poll** path
(`--mode echo-busy`) is the opposite topology: `--threads N` pins **N cores**,
each running one `CoreDriver` that spins a single shared CQ pair at 100 % and
demuxes completions to its connections by `qp_num` (§4.1 of the busy-poll
design). `connections` shard round-robin across the N cores (`conns_per_core =
⌈connections / N⌉`); `in_flight` is the per-connection pipeline depth. So here
`threads` is a **hard core budget** (N fully-consumed cores), not a scheduler
hint — the interesting question is throughput *per pinned core* and where the
shared NIC saturates.

Measured 64 B, `duration=10 warmup=3`, reboot-clean NIC:

| cores | conns | /core | in-flight | throughput | CPU/op | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 2 | 8 | 4 | 16 | 1.91M | 1.05 µs | 64 µs | 94 µs |
| 2 | 8 | 4 | 64 | 2.26M | **0.89 µs** | 192 µs | 838 µs |
| 4 | 16 | 4 | 64 | 3.87M | 1.03 µs | 210 µs | 1338 µs |
| 8 | 32 | 4 | 64 | 5.02M | 1.59 µs | 254 µs | 841 µs |
| 8 | 16 | 2 | 128 | 5.02M | 1.59 µs | 299 µs | 958 µs |
| **8** | **8** | **1** | **256** | **5.08M** | 1.57 µs | 359 µs | **630 µs** |
| 16 | 64 | 4 | 64 | **5.39M** | 2.97 µs | 427 µs | 2403 µs |

**Per-core scaling is near-linear at low core counts, then the shared NIC
message rate saturates.** Two cores sustain 2.26M (≈1.13M/core); four hold 3.87M
(≈0.97M/core); by eight the per-core share has fallen to ~0.63M as the NIC's
per-message completion rate — not the CPUs — becomes the limiter. Beyond eight
cores throughput barely moves (5.0M → 5.4M from 8 → 16 cores) while CPU/op nearly
doubles (1.57 → 2.97 µs) and p99 quadruples (630 µs → 2403 µs): those extra eight
pinned cores spin for a ~7 % throughput gain. **The busy-poll 64 B ceiling is
~5.4M rps; the *efficient* ceiling is ~5.0M on 8 cores.**

**At the ceiling, per-core pipeline *shape* is irrelevant — only the aggregate
offered load matters.** At 8 cores, 4 conns/core × 64, 2 conns/core × 128, and
1 conn/core × 256 all land at ~5.0M. The 1-conn/core deep pipeline gives the
**cleanest tail** (p99 630 µs vs 841–958 µs) at equal throughput, because a
single connection per shared CQ removes cross-connection demux jitter — the
preferred busy config when latency matters.

**Shared-CQ sizing caps per-core depth (§7.2).** Each core's shared CQ is sized
`(conns_per_core + 1) × per_conn_wrs` and must fit the device `max_cqe` (2048 on
MANA). At `in_flight=256`, 4 conns/core needs `send=2575 > 2048` and the pool
**refuses to start** (`InvalidArg: reduce conns_per_core / ring_capacity /
max_in_flight`) — a deliberate admission guard, not a crash. Deeper pipelines
therefore require **fewer connections per core** (drop to 1–2 conns/core for
`in_flight ≥ 128`).

**Busy-poll vs arm-park.** Arm-park read-ring peaks higher (~6.75–6.8M, §above)
because its completions fan out across up to ~9 of the 64 vCPUs on demand; busy
caps at ~5.4M on its pinned cores because each shared CQ is reaped by exactly one
spinning core and that per-CQ completion path will not parallelize further.
Busy-poll trades a slice of peak throughput for **pinned-core determinism and
low-concurrency latency/efficiency**: two dedicated cores already deliver 2.26M
at **0.89 µs/op** with a 192 µs p50 — the regime it is built for (cf. the Slice D4
headline, `echo-busy` 938K vs arm-park `echo` 386K at 8 conns / in-flight 4,
where busy wins 2.4× at low concurrency). Pick busy-poll when you want a fixed,
isolated core budget and predictable tail latency; pick arm-park when you want
the last ~25 % of headline throughput and elastic core use.

## Thread-per-core arm-park (`echo-park`) — the third topology

`echo-park` (`--mode echo-park`) is the **interrupt-driven** sibling of
`echo-busy`: the same `--threads N` pinned `current_thread` cores and the same
round-robin connection sharding, but each connection keeps its own
**interrupt-armed** CQ (like the default `echo` path) instead of a shared
busy-polled CQ. Each connection's completion-channel and CM fds bind to their
owning core's reactor at construction, so the event-driven data path is serviced
on that core — reactor-per-core locality — but the core **parks in `epoll_wait`
when idle** (0 % idle CPU) rather than spinning. It is the middle point between
the two existing modes:

| mode | reaping | idle CPU | core placement | fan-out |
|---|---|---|---|---|
| `echo` (arm-park) | per-conn armed CQ | 0 % (parks) | tokio work-stealing over all vCPUs | elastic (~7–9 cores) |
| `echo-busy` | shared CQ, 100 % spin | **100 %** | pinned 1 core/CQ, sole reaper | none |
| `echo-park` | per-conn armed CQ | **0 % (parks)** | **pinned** 1 core/runtime | none (no stealing) |

Measured 64 B, `duration=10 warmup=3`, reboot-clean NIC, same configs as the
busy sweep:

| cores | conns | /core | in-flight | throughput | CPU/op | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 2 | 8 | 4 | 16 | 1.59M | 1.26 µs | 79 µs | 122 µs |
| 2 | 8 | 4 | 64 | 2.15M | 0.93 µs | 229 µs | **495 µs** |
| 4 | 16 | 4 | 64 | 3.36M | 1.17 µs | 237 µs | 1227 µs |
| 8 | 32 | 4 | 64 | **4.93M** | 1.52 µs | 362 µs | 1634 µs |
| 8 | 8 | 1 | 256 | 4.79M | 1.45 µs | 343 µs | 1110 µs |

`echo-park` **tracks `echo-busy` ~5–15 % behind at every point and converges to
the same ~5M ceiling by 8 cores** (4.93M vs 5.02M) — the shared NIC message rate
caps both. It pays busy-poll's avoided hot-path cost (`ibv_get_cq_event` + re-arm
+ epoll wakeup), so its per-op latency runs a bit higher — except it actually
**wins the tail at low core counts** (2-core p99 495 µs vs busy's 838 µs), the
armed CQ absorbing bursts more evenly than the spin loop's fixed sweep cadence.
The 16-core / 64-connection point could not be measured: `echo-park` repeatedly
failed to *establish* all 64 connections (`expected Established, got Unreachable`)
— the MANA RoCEv2 **CM-setup** flakiness at high fan-out (§ connection ceiling
above), not a throughput limit. Neither pool retries a hung/failed connect, so at
64 connections this surfaces as a run-ending timeout; the ceiling is already
reached at 8 cores regardless.

### Matched-core comparison: same core budget, three topologies

The tables above compare `echo-busy`/`echo-park` head-to-head, but the fair
question is how the pinned modes compare to the **default `echo`** at the *same
core budget*. Running base `echo` (multi-thread arm-park, work-stealing) with
`--threads N` = the same `N` and the same conns/in-flight (64 B, deep
`in_flight=64`, reboot-clean NIC):

| cores | conns | `echo` | `echo-busy` | `echo-park` |
|---:|---:|---:|---:|---:|
| 2 | 8 | **2.59M** (0.75 µs/op, p99 359 µs) | 2.26M (0.89, 838) | 2.15M (0.93, 495) |
| 4 | 16 | **4.42M** (0.87 µs/op, p99 455 µs) | 3.87M (1.03, 1338) | 3.36M (1.17, 1227) |
| 8 | 32 | **5.61M** (1.12 µs/op, p99 1228 µs) | 5.02M (1.59, 841) | 4.93M (1.52, 1634) |

**At a deep pipeline, plain `echo` is the fastest and most CPU-efficient of the
three even at a matched core budget** (5.61M vs 5.02M vs 4.93M on 8 cores, at
0.75–1.12 µs/op vs ~1.5 µs). That is the *opposite* of the shallow-pipeline
result below, and the reason is pipeline depth: at `in_flight=64` the per-message
interrupt cost is already amortized across ~64 completions per wakeup, so
busy-poll's spin buys almost nothing — and it *costs* the shared-CQ demux plus
pinning away work-stealing's ability to smooth bursts across cores (note `echo`
holds ~6.3 busy cores at 8 threads, not a hard 8, and still wins). Busy-poll and
park pin every core at ~100 %/parked-hot for *less* throughput here.

### Idle / shallow pipeline — where busy-poll wins

The pinned modes' advantage is the **shallow-pipeline / low-latency** regime,
where each message pays a full interrupt round-trip that busy-poll's spin
eliminates. At 2 connections / in-flight 1 on 2 cores:

| mode | throughput | cores busy | CPU/op | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| `echo-busy` | **68.6k** | ~2.0 | 29.1 µs | **28 µs** | 34 µs |
| `echo-park` | 28.8k | ~0.5 | 16.7 µs | 67 µs | 80 µs |
| `echo` | 23.7k | ~0.4 | 18.6 µs | 89 µs | 104 µs |

Here busy-poll **wins throughput and latency ~2.4–2.9×** (68.6k / p50 28 µs) —
no per-message `ibv_get_cq_event`/re-arm, it just spins and reaps — at the cost
of pinning 2 whole cores. `echo-park` and `echo` are both interrupt-bound and
much slower per message (p50 67–89 µs), but use only ~0.4–0.5 cores; `echo-park`
edges `echo` on latency (pinned locality, no cross-core task migration) at
slightly higher CPU. This is the crossover: **busy-poll's spin pays off only when
the pipeline is too shallow to amortize the interrupt** — exactly the
request/response regime (`in_flight` 1) the busy-poll design targets (cf. the
Slice D4 headline, `echo-busy` 938K vs arm-park `echo` 386K at 8 conns /
in-flight 4).

### Picking a mode

The three modes hit the same **architectural ~5–6.8M read-ring echo ceiling**
(the per-message doorbell/completion path, not CPU or NIC); which one is best
depends on **pipeline depth and what you value**:

- **`echo`** (default, work-stealing) — **best deep-pipeline throughput and
  CPU/op**, even at a matched core budget; parks when idle. Downside: connections
  migrate across cores (no locality/determinism), and it is interrupt-bound at
  shallow depth. The default choice for raw throughput.
- **`echo-busy`** (pinned, 100 % spin) — **wins the shallow / low-latency
  regime** decisively (p50 28 µs, 2.4× the throughput at `in_flight=1`) and gives
  a hard isolated core budget, at the cost of burning every pinned core
  continuously (even idle) and *losing* on deep-pipeline throughput. Best for
  latency-critical, dedicated-core, request/response workloads.
- **`echo-park`** (pinned, reactor-per-core) — the **middle**: pinned locality
  and determinism with **0 % idle CPU** and the best tail at low core counts, but
  ~5–15 % less throughput than `echo` and higher p50 than `echo-busy`. Best when
  you want core affinity **without** paying for spin on an idle/bursty workload.



