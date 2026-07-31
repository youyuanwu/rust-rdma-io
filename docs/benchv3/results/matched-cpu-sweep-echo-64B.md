# Open-loop matched-throughput CPU sweep — echo · 64 B (read-ring vs kernel TCP)

Fix the offered rate, give each transport its efficient config, and compare CPU + latency
as the matched rate rises. Unlike the fixed in-flight-64 matched board (a single 250k/1M
point), this sweep uses a **deep pipeline** so read-ring runs in its efficient regime, and it
shows the CPU-efficiency gap **growing with load** (per-op doorbell/completion cost amortizes
as the rate climbs; kernel TCP stays CPU-bound at ~4–4.9 µs/op).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 1× vCPU (64) · **in-flight (capacity):** 512 (deep pipeline) · **threads:** 64 · **load:** open-loop matched-throughput · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260731T00Z`

| target rps | transport | achieved rps | p50 (µs) | p99 (µs) | CPU/op (µs) | cores | errors | vs TCP cores |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 1,000,000 | `read-ring` (arm-park) | 1000010 | 406.0 | 1067.0 | 3.58 | 3.58 | 0 | **0.87×** (−13%) |
| 1,000,000 | `read-ring` (busy-poll)† | 1000000 | 494.0 | 980.0 | 64.00 | 64.00 | 0 | tail champion, 64 cores |
| 1,000,000 | kernel baseline | 1000015 | 929.0 | 2044.0 | 4.13 | 4.13 | 6 | baseline |
| 2,000,000 | `read-ring` (arm-park) | 2000038 | 506.0 | 1642.0 | 2.58 | 5.17 | 0 | **0.65×** (−35%) |
| 2,000,000 | `read-ring` (busy-poll)† | 2000003 | 306.0 | 1424.0 | 32.00 | 63.99 | 0 | tail champion, 64 cores |
| 2,000,000 | kernel baseline | 1999980 | 947.0 | 2105.0 | 3.96 | 7.92 | 0 | baseline |
| 3,000,000 | `read-ring` (arm-park) | 2963771 | 555.0 | 2645.0 | 2.33 | 6.91 | 0 | **0.53×** (−47%, ~1.9× TCP) |
| 3,000,000 | `read-ring` (busy-poll)† | 2999990 | 313.0 | 1474.0 | 21.33 | 63.99 | 0 | tail champion, 64 cores |
| 3,000,000 | kernel baseline | 2999948 | 977.0 | 2333.0 | 4.37 | 13.10 | 0 | baseline |

† `read-ring` busy-poll uses **in-flight 256** (in-flight 512 exceeds the device `max_cqe=2048`
shared-CQ limit); it spins one core per connection, so it always pins all 64 cores regardless of
rate — the CPU cost is fixed and its `cores` column is not comparable to the arm-park / TCP rows.

`cores = CPU-seconds / wall-time = CPU-per-op × achieved throughput`. read-ring at 3M achieved
2.96M (98.8% of target, 0 errors) — treated as the 3M point.

## Reading it

- **CPU efficiency grows with load.** read-ring uses 13% fewer cores than TCP at 1M, 35% at 2M,
  and **~47% (nearly 2×)** at 3M. read-ring's per-op cost falls (3.58 → 2.33 µs) as the rate
  amortizes its doorbell/completion overhead; TCP stays ~4–4.4 µs/op (kernel-bound).
- **Latency.** read-ring p50 is ~half of TCP's across the sweep (406–555 µs vs 929–977 µs). Its
  p99 leads TCP up to 2M, then inflates near its own ceiling at 3M (2645 vs 2333 µs).
- **busy-poll is the tail champion** (†): p99 980–1474 µs — well below TCP (2044–2333) and arm-park
  (1067–2645), and it does not inflate at 3M because it sustains the rate by spinning. The cost is
  a fixed **64 cores** at any rate, so it's a latency-at-any-cost option, not a CPU comparison.
- **This is iso-throughput**, unlike the peak-for-peak "~6-vs-36 cores" headline from the
  closed-loop message-rate sweep (`../../bench/azure-mana-rocev2/echo/message-rate-64b.md`).

## Ceiling notes (why 3M is the top matched point)

- **read-ring** tops out at **~3.55M** open-loop at 64 connections (best config, in-flight 512,
  0 errors); more connections hurt (128 conn → 2.66M). Its 6.75M v1 figure was closed-loop
  peak-finding at 32×512, not sustained open-loop.
- **kernel TCP** sustains 4M cleanly (~18 cores, CPU/op ~4.6) — it scales on cores, not depth.
- **send-recv** and **credit-ring** collapse well before 4M (send-recv → ~6k rps at a 4M target;
  credit-ring caps ~1.3M), so the high-rate matched comparison is read-ring vs kernel TCP.
