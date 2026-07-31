# Open-loop matched-throughput CPU sweep — echo · 8 KiB (read-ring vs kernel TCP)

The 8 KiB companion to [matched-cpu-sweep-echo-64B.md](matched-cpu-sweep-echo-64B.md). 8 KiB is
bandwidth-bound, so the rates are much lower and the story is CPU-per-byte at a matched offered
load. **read-ring's ceiling here is a real limit:** in open-loop it over-queues and collapses
above ~120k req/s, so this sweep matches read-ring vs kernel TCP across read-ring's clean range
(60k / 100k / 120k). read-ring's headline 549k / 36 Gbps 8 KiB figure is **closed-loop
peak-finding only** (`../../bench/azure-mana-rocev2/echo/large-payload-8kib.md`).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 1× vCPU (64) · **in-flight (capacity):** 64 · **threads:** 64 · **ring_max_msg:** 8192 · **load:** open-loop matched-throughput · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260731T01Z`

| target rps | transport | achieved rps | p50 (µs) | p99 (µs) | CPU/op (µs) | cores | Gbps | errors | vs TCP cores |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 60,000 | `read-ring` (arm-park) | 60005 | 915.0 | 2271.0 | 12.78 | 0.77 | 3.9 | 0 | **0.71×** (−29%) |
| 60,000 | kernel baseline | 60000 | 1031.0 | 2195.0 | 17.95 | 1.08 | 3.9 | 9 | baseline |
| 100,000 | `read-ring` (arm-park) | 99992 | 767.0 | 1692.0 | 11.11 | 1.11 | 6.6 | 0 | **0.71×** (−29%) |
| 100,000 | kernel baseline | 100000 | 993.0 | 1921.0 | 15.68 | 1.57 | 6.6 | 0 | baseline |
| 120,000 | `read-ring` (arm-park) | 119992 | 790.0 | 1696.0 | 10.51 | 1.26 | 7.9 | 0 | **0.72×** (−28%) |
| 120,000 | kernel baseline | 119994 | 986.0 | 1950.0 | 14.48 | 1.74 | 7.9 | 0 | baseline |

## Reading it

- **read-ring uses ~28–29% fewer cores than TCP across its whole clean range** — a steady edge
  (CPU/op ~10.5–12.8 µs vs TCP ~14.5–18 µs, ~1.4×), unlike the 64 B sweep where the gap *widens*
  with load. At 8 KiB the gap is roughly constant because both are bandwidth-bound at these rates.
- **Latency.** read-ring p50 ~20–25% lower (767–915 vs 986–1031 µs); p99 comparable-or-lower.
- **read-ring 0 errors; kernel TCP logs a few** at the lower 8 KiB rates (9 at 60k, transient).

## Ceiling notes (the read-ring 8 KiB open-loop limit)

- **read-ring** achieves the target cleanly up to **~120k req/s** (0 errors). At **140k it
  collapses** to ~47k with multi-second latency, and deeper/shallower in-flight (16, 128) or
  fewer connections (24) collapse it earlier — it over-queues the 8 KiB ring. So ~120k is the
  max clean open-loop rate.
- **kernel TCP** scales on to **~450k req/s / ~29.5 Gbps** (its bandwidth wall) at ~4 cores.
- **read-ring's 549k / 36 Gbps** 8 KiB result is **closed-loop** peak-finding (24×16), not a
  sustained open-loop point — see `../../bench/azure-mana-rocev2/echo/large-payload-8kib.md`.
- Net: at 8 KiB, read-ring is more CPU-efficient per op **within its rate ceiling**; TCP wins the
  absolute open-loop throughput, and read-ring's raw bandwidth win is a closed-loop result.
