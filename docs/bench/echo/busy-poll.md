# Echo busy-poll ceiling (`echo-busy`)

Throughput and CPU cost of the pinned busy-poll topology (`--mode echo-busy`):
`--threads N` pins N cores, each spinning one shared CQ at 100 %. See
[README.md](README.md) for the scenario. Other echo datasets:
[message-rate (64 B)](message-rate-64b.md) ·
[large-payload (8 KiB)](large-payload-8kib.md) ·
[thread-per-core (echo-park)](thread-per-core-park.md).

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

## Re-validation (2026-07-17)

Re-run on the current binary (64 B, `duration=10 warmup=3`, reboot-clean NIC) as
a regression check. **No regression** — being pinned-core deterministic, every
point reproduces within ~1 %:

| cores | conns | in-flight | throughput | CPU/op | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 2 | 8 | 16 | 1.90M (1.91M) | 1.05 µs | 64 µs | 95 µs |
| 2 | 8 | 64 | 2.25M (2.26M) | 0.89 µs | 169 µs | 1368 µs |
| 4 | 16 | 64 | 3.86M (3.87M) | 1.04 µs | 202 µs | 1820 µs |
| 8 | 32 | 64 | 5.01M (5.02M) | 1.60 µs | 246 µs | 884 µs |
| 8 | 16 | 128 | 5.01M (5.02M) | 1.60 µs | 295 µs | 970 µs |
| 8 | 8 | 256 | 5.07M (5.08M) | 1.58 µs | 359 µs | 633 µs |
| 16 | 64 | 64 | 5.39M (5.39M) | 2.97 µs | 423 µs | 2381 µs |

The ~5.0M efficient / ~5.4M peak ceiling, the per-core scaling knee, and the
idle-load point (2 cores / 2 conns / in-flight 1 → 68.5k, 0.89 µs/op after
warmup, p50 28 µs — baseline 68.6k) all hold.
