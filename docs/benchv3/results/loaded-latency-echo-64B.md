# Open-loop loaded tail-latency — echo · 64 B

Achieved-vs-target rate and the p50/p99/p99.9 distribution as the offered rate rises (1× vCPU / 64 conn, in-flight-64 capacity). See the [scenario matrix](../scenario-matrix.md#offered-load-scenarios-open-loop).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 1× vCPU · **load:** open-loop loaded-latency · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260728T182730Z`

| transport | target rps | achieved rps | p50 (µs) | p99 (µs) | p99.9 (µs) | CPU/op (µs) | cores | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `send-recv` | 100000 | 100001 | 761.0 | 1641.0 | 1802.0 | 10.66 | 1.07 | 0 |
| `send-recv` | 250000 | 249985 | 642.0 | 1953.0 | 2211.0 | 9.00 | 2.25 | 0 |
| `send-recv` | 500000 | 500005 | 383.0 | 771.0 | 879.0 | 5.87 | 2.94 | 0 |
| `send-recv` | 1000000 | 999990 | 424.0 | 936.0 | 1164.0 | 5.11 | 5.11 | 0 |
| `send-recv` | 2000000 | 2000047 | 456.0 | 1083.0 | 1323.0 | 2.77 | 5.54 | 0 |
| `read-ring` (arm-park) | 100000 | 99999 | 759.0 | 1612.0 | 1733.0 | 8.95 | 0.89 | 0 |
| `read-ring` (arm-park) | 250000 | 250023 | 604.0 | 2051.0 | 2339.0 | 9.47 | 2.37 | 0 |
| `read-ring` (arm-park) | 500000 | 500004 | 389.0 | 749.0 | 852.0 | 6.16 | 3.08 | 0 |
| `read-ring` (arm-park) | 1000000 | 1000008 | 411.0 | 988.0 | 1143.0 | 3.38 | 3.38 | 0 |
| `read-ring` (arm-park) | 2000000 | 1999982 | 491.0 | 1455.0 | 2209.0 | 2.50 | 5.00 | 0 |
| `read-ring` (busy-poll) | 100000 | 100000 | 500.0 | 992.0 | 3013.0 | 638.99 | 63.90 | 0 |
| `read-ring` (busy-poll) | 250000 | 207031 | 499.0 | 990.0 | 1001.0 | 309.04 | 63.98 | 11 |
| `read-ring` (busy-poll) | 500000 | 500001 | 497.0 | 988.0 | 999.0 | 127.90 | 63.95 | 0 |
| `read-ring` (busy-poll) | 1000000 | 1000001 | 495.0 | 980.0 | 995.0 | 63.98 | 63.98 | 0 |
| `read-ring` (busy-poll) | 2000000 | 2000021 | 312.0 | 1447.0 | 2053.0 | 32.00 | 63.99 | 0 |
| `read-ring` (thread-per-core park) | 100000 | 97861 | 788.0 | 2022.0 | 2103.0 | 13.50 | 1.32 | 1 |
| `read-ring` (thread-per-core park) | 250000 | 250009 | 1004.0 | 2705.0 | 3455.0 | 9.50 | 2.38 | 0 |
| `read-ring` (thread-per-core park) | 500000 | 375011 | 760.0 | 2255.0 | 2941.0 | 9.98 | 3.74 | 15 |
| `read-ring` (thread-per-core park) | 1000000 | 732392 | 720.0 | 4423.0 | 32127.0 | 22.48 | 16.46 | 2 |
| `read-ring` (thread-per-core park) | 2000000 | 819572 | 415.0 | 132991.0 | 611327.0 | 24.35 | 19.96 | 20 |
| `credit-ring` | 100000 | 100005 | 748.0 | 1579.0 | 1685.0 | 7.62 | 0.76 | 0 |
| `credit-ring` | 250000 | 250002 | 740.0 | 1989.0 | 2191.0 | 6.40 | 1.60 | 0 |
| `credit-ring` | 500000 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 1000000 | 1000020 | 345.0 | 689.0 | 885.0 | 3.84 | 3.84 | 0 |
| `credit-ring` | 2000000 | 969149 | 4112383.0 | 6803455.0 | 6897663.0 | 3.36 | 3.26 | 0 |
| kernel baseline | 100000 | 100002 | 934.0 | 1759.0 | 1895.0 | 10.71 | 1.07 | 28 |
| kernel baseline | 250000 | 250002 | 923.0 | 1860.0 | 2057.0 | 6.50 | 1.63 | 0 |
| kernel baseline | 500000 | 499991 | 916.0 | 1860.0 | 2093.0 | 4.39 | 2.20 | 48 |
| kernel baseline | 1000000 | 999942 | 942.0 | 2069.0 | 2297.0 | 4.15 | 4.15 | 0 |
| kernel baseline | 2000000 | 2000151 | 922.0 | 2015.0 | 2221.0 | 3.59 | 7.17 | 0 |
