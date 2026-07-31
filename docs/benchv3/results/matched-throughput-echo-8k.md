# Open-loop matched throughput — echo · 8 KiB

CPU cost and latency of every transport at one shared 150k req/s offered load (1× vCPU / 64 conn, 8 KiB ≈ 9.8 Gbps).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight (capacity):** 64 · **load:** open-loop matched-throughput · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260728T211843Z`

| transport | target rps | achieved rps | p50 (µs) | p99 (µs) | p99.9 (µs) | CPU/op (µs) | cores | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `send-recv` | 150000 | 149987 | 809.0 | 2007.0 | 2267.0 | 13.07 | 1.96 | 0 |
| `read-ring` (arm-park) | 150000 | 128452 | 873.0 | 3271.0 | 1252351.0 | 28.18 | 3.62 | 0 |
| `read-ring` (busy-poll) | 150000 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | 150000 | 149885 | 1100.0 | 376319.0 | 702463.0 | 23.12 | 3.47 | 0 |
| `credit-ring` | 150000 | 150000 | 727.0 | 1805.0 | 1937.0 | 7.59 | 1.14 | 0 |
| kernel baseline | 150000 | 150000 | 988.0 | 1983.0 | 2151.0 | 12.49 | 1.87 | 0 |
