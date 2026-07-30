# Open-loop matched throughput — echo · 64 B

CPU cost and latency of every transport at one shared 250k req/s offered load (1× vCPU / 64 conn).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 1× vCPU · **in-flight (capacity):** 64 · **load:** open-loop matched-throughput · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260728T182612Z`

| transport | target rps | achieved rps | p50 (µs) | p99 (µs) | p99.9 (µs) | CPU/op (µs) | cores | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `send-recv` | 250000 | 249985 | 642.0 | 1953.0 | 2211.0 | 9.00 | 2.25 | 0 |
| `read-ring` (arm-park) | 250000 | 250023 | 604.0 | 2051.0 | 2339.0 | 9.47 | 2.37 | 0 |
| `read-ring` (busy-poll) | 250000 | 207031 | 499.0 | 990.0 | 1001.0 | 309.04 | 63.98 | 11 |
| `read-ring` (thread-per-core park) | 250000 | 250009 | 1004.0 | 2705.0 | 3455.0 | 9.50 | 2.38 | 0 |
| `credit-ring` | 250000 | 250002 | 740.0 | 1989.0 | 2191.0 | 6.40 | 1.60 | 0 |
| kernel baseline | 250000 | 250002 | 923.0 | 1860.0 | 2057.0 | 6.50 | 1.63 | 0 |
