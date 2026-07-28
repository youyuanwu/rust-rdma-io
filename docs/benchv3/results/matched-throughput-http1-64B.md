# Open-loop matched throughput — HTTP/1.1 · 64 B

Every HTTP/1.1 transport at one shared 100k req/s offered load (1× vCPU / 64 conn).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **connections:** 1× vCPU · **load:** open-loop matched-throughput · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260728T192015Z`

| transport | target rps | achieved rps | p50 (µs) | p99 (µs) | p99.9 (µs) | CPU/op (µs) | cores | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `send-recv` | 100000 | 100005 | 904.0 | 1825.0 | 2039.0 | 21.78 | 2.18 | 0 |
| `read-ring` (arm-park) | 100000 | 100011 | 872.0 | 1791.0 | 1985.0 | 21.50 | 2.15 | 0 |
| `read-ring` (busy-poll) | 100000 | 100001 | 501.0 | 993.0 | 1004.0 | 639.72 | 63.97 | 0 |
| `read-ring` (thread-per-core park) | 100000 | 100001 | 769.0 | 2022.0 | 2105.0 | 23.18 | 2.32 | 0 |
| `credit-ring` | 100000 | 99996 | 908.0 | 1870.0 | 2075.0 | 21.99 | 2.20 | 0 |
| kernel baseline | 100000 | 99995 | 843.0 | 1766.0 | 1949.0 | 23.31 | 2.33 | 0 |
