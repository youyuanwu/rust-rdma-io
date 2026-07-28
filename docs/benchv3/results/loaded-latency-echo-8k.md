# Open-loop loaded tail-latency — echo · 8 KiB

Bandwidth-bound regime: achieved-vs-target rate and p50/p99/p99.9 as the offered rate rises (1× vCPU / 64 conn). Ring transports use ring_max_msg=8192 (echo). See the [scenario matrix](../scenario-matrix.md#offered-load-scenarios-open-loop).

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 1× vCPU · **load:** open-loop loaded-latency · **duration/warmup:** 10 s / 3 s · **git commit:** `2bbfd3d` · **date:** `20260728T210035Z`

| transport | target rps | achieved rps | p50 (µs) | p99 (µs) | p99.9 (µs) | CPU/op (µs) | cores | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `send-recv` | 50000 | 50000 | 982.0 | 2113.0 | 2299.0 | 12.96 | 0.65 | 0 |
| `send-recv` | 100000 | 100006 | 795.0 | 1754.0 | 1891.0 | 11.21 | 1.12 | 0 |
| `send-recv` | 200000 | 200005 | 728.0 | 1967.0 | 2211.0 | 10.15 | 2.03 | 0 |
| `send-recv` | 300000 | 300037 | 455.0 | 1802.0 | 2343.0 | 16.57 | 4.97 | 0 |
| `send-recv` | 400000 | 399996 | 348.0 | 1163.0 | 1401.0 | 10.83 | 4.33 | 0 |
| `read-ring` (arm-park) | 50000 | 50005 | 985.0 | 2057.0 | 2289.0 | 12.66 | 0.63 | 0 |
| `read-ring` (arm-park) | 100000 | 100000 | 785.0 | 1724.0 | 1859.0 | 10.98 | 1.10 | 0 |
| `read-ring` (arm-park) | 200000 | 6200 | 4734975.0 | 9625599.0 | 9740287.0 | 1073.73 | 6.66 | 0 |
| `read-ring` (arm-park) | 300000 | 6403 | 7868415.0 | 12525567.0 | 12664831.0 | 1042.59 | 6.68 | 0 |
| `read-ring` (arm-park) | 400000 | 6565 | 5083135.0 | 9977855.0 | 10125311.0 | 1061.19 | 6.97 | 0 |
| `read-ring` (busy-poll) | 50000 | 49219 | 536.0 | 1030.0 | 1040.0 | 1300.00 | 63.98 | 1 |
| `read-ring` (busy-poll) | 100000 | 100000 | 497.0 | 990.0 | 1001.0 | 639.87 | 63.99 | 0 |
| `read-ring` (busy-poll) | 200000 | 200000 | 486.0 | 987.0 | 1008.0 | 319.94 | 63.99 | 0 |
| `read-ring` (busy-poll) | 300000 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (busy-poll) | 400000 | 400001 | 476.0 | 965.0 | 2341.0 | 159.80 | 63.92 | 0 |
| `read-ring` (thread-per-core park) | 50000 | 49999 | 939.0 | 2007.0 | 2111.0 | 21.02 | 1.05 | 0 |
| `read-ring` (thread-per-core park) | 100000 | 98438 | 780.0 | 2014.0 | 2117.0 | 15.64 | 1.54 | 1 |
| `read-ring` (thread-per-core park) | 200000 | 199728 | 1555.0 | 192127.0 | 349951.0 | 24.09 | 4.81 | 0 |
| `read-ring` (thread-per-core park) | 300000 | 292870 | 118783.0 | 332287.0 | 443647.0 | 14.06 | 4.12 | 1 |
| `read-ring` (thread-per-core park) | 400000 | 319660 | 1659903.0 | 2768895.0 | 2818047.0 | 13.41 | 4.29 | 0 |
| `credit-ring` | 50000 | 49223 | 941.0 | 2075.0 | 2245.0 | 25.58 | 1.26 | 1 |
| `credit-ring` | 100000 | 100000 | 784.0 | 1747.0 | 1882.0 | 11.40 | 1.14 | 0 |
| `credit-ring` | 200000 | 200001 | 795.0 | 1991.0 | 2171.0 | 6.75 | 1.35 | 0 |
| `credit-ring` | 300000 | 295756 | 462.0 | 2185.0 | 2475.0 | 10.82 | 3.20 | 0 |
| `credit-ring` | 400000 | 400007 | 321.0 | 1169.0 | 1346.0 | 7.17 | 2.87 | 0 |
| kernel baseline | 50000 | 49995 | 1112.0 | 2229.0 | 2419.0 | 18.50 | 0.93 | 53 |
| kernel baseline | 100000 | 100002 | 978.0 | 1862.0 | 2012.0 | 14.74 | 1.47 | 62 |
| kernel baseline | 200000 | 200001 | 944.0 | 2003.0 | 2153.0 | 10.85 | 2.17 | 56 |
| kernel baseline | 300000 | 299972 | 973.0 | 2131.0 | 2315.0 | 10.23 | 3.07 | 48 |
| kernel baseline | 400000 | 400001 | 1222.0 | 2415.0 | 2619.0 | 9.16 | 3.67 | 1 |
