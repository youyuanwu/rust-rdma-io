# Open-loop loaded tail-latency — HTTP/1.1 · 64 B

HTTP/1.1 (one request per connection; achievable rate bounded by connections/RTT). 1× vCPU / 64 conn.

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **connections:** 1× vCPU · **in-flight (capacity):** 1 · **load:** open-loop loaded-latency · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260728T192015Z`

| transport | target rps | achieved rps | p50 (µs) | p99 (µs) | p99.9 (µs) | CPU/op (µs) | cores | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `send-recv` | 25000 | 24997 | 1236.0 | 2273.0 | 2393.0 | 23.56 | 0.59 | 0 |
| `send-recv` | 50000 | 49998 | 1030.0 | 2207.0 | 2411.0 | 23.72 | 1.19 | 0 |
| `send-recv` | 100000 | 100005 | 904.0 | 1825.0 | 2039.0 | 21.78 | 2.18 | 0 |
| `send-recv` | 150000 | 150008 | 972.0 | 2002.0 | 2183.0 | 17.98 | 2.70 | 0 |
| `send-recv` | 200000 | 200003 | 607.0 | 1589.0 | 2217.0 | 19.26 | 3.85 | 0 |
| `read-ring` (arm-park) | 25000 | 25004 | 1401.0 | 2237.0 | 2349.0 | 21.92 | 0.55 | 0 |
| `read-ring` (arm-park) | 50000 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | 100000 | 100011 | 872.0 | 1791.0 | 1985.0 | 21.50 | 2.15 | 0 |
| `read-ring` (arm-park) | 150000 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | 200000 | 199999 | 808.0 | 2151.0 | 2355.0 | 19.20 | 3.84 | 0 |
| `read-ring` (busy-poll) | 25000 | 24610 | 555.0 | 1049.0 | 1062.0 | 2600.28 | 63.99 | 1 |
| `read-ring` (busy-poll) | 50000 | 50000 | 563.0 | 1051.0 | 1059.0 | 1279.91 | 64.00 | 0 |
| `read-ring` (busy-poll) | 100000 | 100001 | 501.0 | 993.0 | 1004.0 | 639.72 | 63.97 | 0 |
| `read-ring` (busy-poll) | 150000 | 0 | 0.0 | 0.0 | 0.0 |  | 31.99 | 64 |
| `read-ring` (busy-poll) | 200000 | 0 | 0.0 | 0.0 | 0.0 |  | 31.99 | 64 |
| `read-ring` (thread-per-core park) | 25000 | 24609 | 1151.0 | 2010.0 | 2109.0 | 29.38 | 0.72 | 1 |
| `read-ring` (thread-per-core park) | 50000 | 49220 | 926.0 | 1997.0 | 2107.0 | 26.70 | 1.31 | 1 |
| `read-ring` (thread-per-core park) | 100000 | 100001 | 769.0 | 2022.0 | 2105.0 | 23.18 | 2.32 | 0 |
| `read-ring` (thread-per-core park) | 150000 | 150003 | 727.0 | 2031.0 | 2183.0 | 23.17 | 3.48 | 0 |
| `read-ring` (thread-per-core park) | 200000 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 25000 | 24998 | 1183.0 | 2301.0 | 2401.0 | 24.84 | 0.62 | 0 |
| `credit-ring` | 50000 | 49996 | 1026.0 | 2233.0 | 2445.0 | 23.76 | 1.19 | 0 |
| `credit-ring` | 100000 | 99996 | 908.0 | 1870.0 | 2075.0 | 21.99 | 2.20 | 0 |
| `credit-ring` | 150000 | 150004 | 953.0 | 2115.0 | 2317.0 | 21.58 | 3.24 | 0 |
| `credit-ring` | 200000 | 199980 | 628.0 | 2089.0 | 2363.0 | 24.30 | 4.86 | 0 |
| kernel baseline | 25000 | 25005 | 1158.0 | 2267.0 | 2381.0 | 26.47 | 0.66 | 0 |
| kernel baseline | 50000 | 50001 | 1004.0 | 2209.0 | 2411.0 | 26.08 | 1.30 | 0 |
| kernel baseline | 100000 | 99995 | 843.0 | 1766.0 | 1949.0 | 23.31 | 2.33 | 0 |
| kernel baseline | 150000 | 150000 | 923.0 | 1924.0 | 2149.0 | 22.59 | 3.39 | 0 |
| kernel baseline | 200000 | 199982 | 838.0 | 2131.0 | 2351.0 | 22.87 | 4.57 | 0 |
