## echo — 64 B

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T222056Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 273358 | 230.0 | 315.0 | 361.0 | 10.91 | 2.98 | 22 | 0 |
| `read-ring` (arm-park) | 238411 | 241.0 | 453.0 | 576.0 | 19.42 | 4.63 | 31 | 0 |
| `read-ring` (busy-poll) | 224449 | 209.0 | 769.0 | 1130.0 | 285.11 | 63.99 | 34 | 1 |
| `read-ring` (thread-per-core park) | 101838 | 614.0 | 654.0 | 763.0 | 21.71 | 2.21 | 33 | 1 |
| `credit-ring` | 357072 | 175.0 | 285.0 | 335.0 | 9.12 | 3.25 | 31 | 0 |
| kernel baseline | 298558 | 208.0 | 312.0 | 361.0 | 12.00 | 3.58 | 16 | 0 |

#### 1× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 1× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T222228Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 4185786 | 352.0 | 1210.0 | 2151.0 | 1.49 | 6.25 | 25 | 0 |
| `read-ring` (arm-park) | 4533514 | 205.0 | 805.0 | 1603.0 | 1.39 | 6.28 | 35 | 0 |
| `read-ring` (busy-poll) | 11485470 | 320.0 | 420.0 | 575.0 | 5.57 | 63.98 | 41 | 4 |
| `read-ring` (thread-per-core park) | 4104185 | 136.0 | 540.0 | 889.0 | 1.83 | 7.50 | 37 | 0 |
| `credit-ring` | 975821 | 4183.0 | 4507.0 | 4631.0 | 3.12 | 3.04 | 35 | 0 |
| kernel baseline | 6293305 | 288.0 | 835.0 | 1548.0 | 5.73 | 36.08 | 16 | 0 |

#### 1× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 1× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T222402Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 10891 | 590.0 | 4599807.0 | 5259263.0 | 1.84 | 0.02 | 50 | 0 |
| `read-ring` (arm-park) | 5080660 | 534.0 | 1987.0 | 2751.0 | 1.06 | 5.37 | 70 | 0 |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | 4332378 | 553.0 | 1784.0 | 2629.0 | 0.80 | 3.46 | 76 | 0 |
| `credit-ring` | 969239 | 33695.0 | 36191.0 | 36671.0 | 3.12 | 3.02 | 74 | 0 |
| kernel baseline | 6883099 | 2725.0 | 6407.0 | 9167.0 | 7.12 | 49.03 | 17 | 0 |

#### 2× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 2× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T173405Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 183150 | 669.0 | 1103.0 | 1348.0 | 40.25 | 7.37 | 32 | 0 |
| `read-ring` (arm-park) | 175738 | 698.0 | 1178.0 | 1456.0 | 39.96 | 7.02 | 48 | 0 |
| `read-ring` (busy-poll) | 289166 | 342.0 | 1124.0 | 1616.0 | 221.27 | 63.99 | 53 | 3 |
| `read-ring` (thread-per-core park) | 107595 | 416.0 | 457.0 | 560.0 | 20.78 | 2.24 | 36 | 83 |
| `credit-ring` | 371940 | 264.0 | 747.0 | 1217.0 | 7.95 | 2.96 | 48 | 0 |
| kernel baseline | 271060 | 400.0 | 825.0 | 1046.0 | 24.09 | 6.53 | 28 | 0 |

#### 2× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 2× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T173632Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 4206260 | 247.0 | 977.0 | 2020.0 | 1.50 | 6.31 | 36 | 0 |
| `read-ring` (arm-park) | 4406701 | 218.0 | 634.0 | 1015.0 | 1.31 | 5.78 | 54 | 0 |
| `read-ring` (busy-poll) | 10273152 | 724.0 | 1326.0 | 1710.0 | 6.23 | 63.99 | 64 | 1 |
| `read-ring` (thread-per-core park) | 0 | 0.0 | 0.0 | 0.0 |  | 0.00 | 32 | 128 |
| `credit-ring` | 913320 | 8743.0 | 9503.0 | 15255.0 | 3.23 | 2.95 | 59 | 0 |
| kernel baseline | 7665440 | 397.0 | 1443.0 | 2647.0 | 6.53 | 50.05 | 28 | 0 |

#### 2× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 2× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T173910Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 20528 | 1351.0 | 5259263.0 | 5259263.0 | 2.97 | 0.06 | 89 | 0 |
| `read-ring` (arm-park) | 5677612 | 464.0 | 1681.0 | 2499.0 | 1.18 | 6.71 | 124 | 0 |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | 0 | 0.0 | 0.0 | 0.0 |  | 0.00 | 76 | 128 |
| `credit-ring` | 918616 | 69887.0 | 74175.0 | 105151.0 | 4.35 | 4.00 | 136 | 0 |
| kernel baseline | 8236427 | 1759.0 | 8255.0 | 11415.0 | 6.40 | 52.73 | 29 | 0 |

#### 4× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 4× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T195213Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | 193192 | 1282.0 | 2197.0 | 2713.0 | 39.05 | 7.54 | 88 | 0 |
| `read-ring` (busy-poll) | 330800 | 630.0 | 1817.0 | 2521.0 | 193.25 | 63.93 | 91 | 7 |
| `read-ring` (thread-per-core park) | 113913 | 2239.0 | 2331.0 | 2391.0 | 16.67 | 1.90 | 92 | 1 |
| `credit-ring` | 369865 | 244.0 | 973.0 | 1831.0 | 8.27 | 3.06 | 84 | 5 |
| kernel baseline | 205204 | 1106.0 | 1874.0 | 2317.0 | 39.23 | 8.05 | 45 | 0 |

#### 4× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **connections:** 4× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T205803Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | 4229620 | 180.0 | 515.0 | 1188.0 | 1.23 | 5.18 | 106 | 2 |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 4× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** echo · **payload:** 64 B · **connections:** 4× vCPU · **in-flight:** 512 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |


### Table B — concurrency grid per transport

#### send-recv

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **transport:** `send-recv` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T165431Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 273358 | 230.0 | 315.0 | 361.0 | 10.91 | 2.98 | 22 | 0 |
| 1× vCPU | 64 | 4185786 | 352.0 | 1210.0 | 2151.0 | 1.49 | 6.25 | 25 | 0 |
| 1× vCPU | 512 | 10891 | 590.0 | 4599807.0 | 5259263.0 | 1.84 | 0.02 | 50 | 0 |
| 2× vCPU | 1 | 183150 | 669.0 | 1103.0 | 1348.0 | 40.25 | 7.37 | 32 | 0 |
| 2× vCPU | 64 | 4206260 | 247.0 | 977.0 | 2020.0 | 1.50 | 6.31 | 36 | 0 |
| 2× vCPU | 512 | 20528 | 1351.0 | 5259263.0 | 5259263.0 | 2.97 | 0.06 | 89 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T210531Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 238411 | 241.0 | 453.0 | 576.0 | 19.42 | 4.63 | 31 | 0 |
| 1× vCPU | 64 | 4533514 | 205.0 | 805.0 | 1603.0 | 1.39 | 6.28 | 35 | 0 |
| 1× vCPU | 512 | 5080660 | 534.0 | 1987.0 | 2751.0 | 1.06 | 5.37 | 70 | 0 |
| 2× vCPU | 1 | 175738 | 698.0 | 1178.0 | 1456.0 | 39.96 | 7.02 | 48 | 0 |
| 2× vCPU | 64 | 4406701 | 218.0 | 634.0 | 1015.0 | 1.31 | 5.78 | 54 | 0 |
| 2× vCPU | 512 | 5677612 | 464.0 | 1681.0 | 2499.0 | 1.18 | 6.71 | 124 | 0 |
| 4× vCPU | 1 | 193192 | 1282.0 | 2197.0 | 2713.0 | 39.05 | 7.54 | 88 | 0 |
| 4× vCPU | 64 | 4229620 | 180.0 | 515.0 | 1188.0 | 1.23 | 5.18 | 106 | 2 |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (busy-poll)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **transport:** `read-ring` (busy-poll) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T171222Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 224449 | 209.0 | 769.0 | 1130.0 | 285.11 | 63.99 | 34 | 1 |
| 1× vCPU | 64 | 11485470 | 320.0 | 420.0 | 575.0 | 5.57 | 63.98 | 41 | 4 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 1 | 289166 | 342.0 | 1124.0 | 1616.0 | 221.27 | 63.99 | 53 | 3 |
| 2× vCPU | 64 | 10273152 | 724.0 | 1326.0 | 1710.0 | 6.23 | 63.99 | 64 | 1 |
| 2× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | 330800 | 630.0 | 1817.0 | 2521.0 | 193.25 | 63.93 | 91 | 7 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (thread-per-core park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **transport:** `read-ring` (thread-per-core park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T172010Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 101838 | 614.0 | 654.0 | 763.0 | 21.71 | 2.21 | 33 | 1 |
| 1× vCPU | 64 | 4104185 | 136.0 | 540.0 | 889.0 | 1.83 | 7.50 | 37 | 0 |
| 1× vCPU | 512 | 4332378 | 553.0 | 1784.0 | 2629.0 | 0.80 | 3.46 | 76 | 0 |
| 2× vCPU | 1 | 107595 | 416.0 | 457.0 | 560.0 | 20.78 | 2.24 | 36 | 83 |
| 2× vCPU | 64 | 0 | 0.0 | 0.0 | 0.0 |  | 0.00 | 32 | 128 |
| 2× vCPU | 512 | 0 | 0.0 | 0.0 | 0.0 |  | 0.00 | 76 | 128 |
| 4× vCPU | 1 | 113913 | 2239.0 | 2331.0 | 2391.0 | 16.67 | 1.90 | 92 | 1 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T173405Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 357072 | 175.0 | 285.0 | 335.0 | 9.12 | 3.25 | 31 | 0 |
| 1× vCPU | 64 | 975821 | 4183.0 | 4507.0 | 4631.0 | 3.12 | 3.04 | 35 | 0 |
| 1× vCPU | 512 | 969239 | 33695.0 | 36191.0 | 36671.0 | 3.12 | 3.02 | 74 | 0 |
| 2× vCPU | 1 | 371940 | 264.0 | 747.0 | 1217.0 | 7.95 | 2.96 | 48 | 0 |
| 2× vCPU | 64 | 913320 | 8743.0 | 9503.0 | 15255.0 | 3.23 | 2.95 | 59 | 0 |
| 2× vCPU | 512 | 918616 | 69887.0 | 74175.0 | 105151.0 | 4.35 | 4.00 | 136 | 0 |
| 4× vCPU | 1 | 369865 | 244.0 | 973.0 | 1831.0 | 8.27 | 3.06 | 84 | 5 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T174344Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 298558 | 208.0 | 312.0 | 361.0 | 12.00 | 3.58 | 16 | 0 |
| 1× vCPU | 64 | 6293305 | 288.0 | 835.0 | 1548.0 | 5.73 | 36.08 | 16 | 0 |
| 1× vCPU | 512 | 6883099 | 2725.0 | 6407.0 | 9167.0 | 7.12 | 49.03 | 17 | 0 |
| 2× vCPU | 1 | 271060 | 400.0 | 825.0 | 1046.0 | 24.09 | 6.53 | 28 | 0 |
| 2× vCPU | 64 | 7665440 | 397.0 | 1443.0 | 2647.0 | 6.53 | 50.05 | 28 | 0 |
| 2× vCPU | 512 | 8236427 | 1759.0 | 8255.0 | 11415.0 | 6.40 | 52.73 | 29 | 0 |
| 4× vCPU | 1 | 205204 | 1106.0 | 1874.0 | 2317.0 | 39.23 | 8.05 | 45 | 0 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

