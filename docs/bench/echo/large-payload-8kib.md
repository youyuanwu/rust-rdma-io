# Echo large-payload (8 KiB) — read-ring vs TCP

Bandwidth-bound 8 KiB request/response throughput, where zero-copy RDMA Writes
overtake the kernel TCP stack. See [README.md](README.md) for the scenario. Other
echo datasets: [message-rate (64 B)](message-rate-64b.md) ·
[busy-poll](busy-poll.md) · [thread-per-core (echo-park)](thread-per-core-park.md).

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

## Re-validation (2026-07-17)

Re-run on the current binary (`payload=8192`, `ring_max_msg=8192` for read-ring,
`duration=10 warmup=3 threads=64`, reboot-clean NIC) as a regression check. **No
regression** — both bandwidth ceilings hold and read-ring still wins 8 KiB.

| transport | config | throughput | bandwidth | p50 | p99 | CPU/op | ~cores |
|---|---|---:|---:|---:|---:|---:|---:|
| **read-ring** | 24 × 8 (peak) | 533k (539k) | **35.0 Gbps** (35.3) | 268 µs | 619 µs | 4.95 µs | ~2.6 |
| **tcp** | 32 × 4 (peak) | 452k (454k) | 29.6 Gbps (29.8) | 262 µs | 443 µs | 8.91 µs | ~4.0 |

read-ring sweep (req/s | Gbps): 8×8 350k/22.9, 16×16 501k/32.9, **24×8
533k/35.0**, 24×16 424k/27.8, 32×16 415k/27.2 — same inverted-U, peak at 24×8
(the 24×16 point landed on the over-queue side this run, run-to-run knee noise).
TCP flat at the ~29.2 Gbps bandwidth wall across 32×8 / 64×8 / 32×16 / 64×16 /
128×16 (all ~446k), latency inflating with concurrency (p50 539 µs → 4.3 ms).
read-ring still leads by ~+18 % bandwidth on fewer cores — unchanged from
baseline.
