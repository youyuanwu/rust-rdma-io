## echo — 8 KiB

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T222143Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 270294 | 17.71 | 233.0 | 312.0 | 353.0 | 12.20 | 3.30 | 25 | 0 |
| `read-ring` (arm-park) | 200083 | 13.11 | 292.0 | 523.0 | 648.0 | 26.57 | 5.32 | 31 | 0 |
| `read-ring` (busy-poll) | 230237 | 15.09 | 203.0 | 726.0 | 1064.0 | 277.89 | 63.98 | 34 | 2 |
| `read-ring` (thread-per-core park) | 99867 | 6.54 | 625.0 | 666.0 | 790.0 | 22.95 | 2.29 | 33 | 1 |
| `credit-ring` | 319239 | 20.92 | 195.0 | 320.0 | 401.0 | 9.03 | 2.88 | 31 | 0 |
| kernel baseline | 257665 | 16.89 | 238.0 | 375.0 | 441.0 | 17.41 | 4.49 | 19 | 0 |

#### 1× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T222315Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 416367 | 27.29 | 9839.0 | 10671.0 | 10943.0 | 9.98 | 4.15 | 88 | 0 |
| `read-ring` (arm-park) | 405540 | 26.58 | 703.0 | 1148.0 | 1305.0 | 9.00 | 3.65 | 35 | 0 |
| `read-ring` (busy-poll) | 404284 | 26.50 | 631.0 | 1076.0 | 1272.0 | 158.24 | 63.97 | 40 | 3 |
| `read-ring` (thread-per-core park) | 330692 | 21.67 | 1067.0 | 6939.0 | 10863.0 | 8.94 | 2.96 | 37 | 0 |
| `credit-ring` | 396014 | 25.95 | 1214.0 | 1572.0 | 1748.0 | 7.62 | 3.02 | 35 | 0 |
| kernel baseline | 445947 | 29.23 | 6351.0 | 20543.0 | 24303.0 | 8.59 | 3.83 | 19 | 0 |

#### 1× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T222515Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 413041 | 27.07 | 76159.0 | 83007.0 | 85823.0 | 9.75 | 4.03 | 559 | 2 |
| `read-ring` (arm-park) | 405978 | 26.61 | 704.0 | 1144.0 | 1302.0 | 9.27 | 3.76 | 74 | 0 |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 396169 | 25.96 | 1125.0 | 1407.0 | 1516.0 | 6.99 | 2.77 | 73 | 0 |
| kernel baseline | 445029 | 29.17 | 25903.0 | 73087.0 | 90367.0 | 9.00 | 4.01 | 20 | 0 |

#### 2× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T173535Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 175909 | 11.53 | 694.0 | 1152.0 | 1415.0 | 42.76 | 7.52 | 38 | 0 |
| `read-ring` (arm-park) | 167248 | 10.96 | 730.0 | 1220.0 | 1503.0 | 43.52 | 7.28 | 48 | 0 |
| `read-ring` (busy-poll) | 274637 | 18.00 | 369.0 | 1197.0 | 1716.0 | 232.95 | 63.98 | 54 | 0 |
| `read-ring` (thread-per-core park) | 107926 | 7.07 | 1144.0 | 1205.0 | 1296.0 | 19.75 | 2.13 | 52 | 4 |
| `credit-ring` | 193340 | 12.67 | 540.0 | 1044.0 | 1294.0 | 37.95 | 7.34 | 46 | 0 |
| kernel baseline | 305599 | 20.03 | 391.0 | 669.0 | 841.0 | 19.77 | 6.04 | 29 | 0 |

#### 2× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T173803Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 405772 | 26.59 | 20111.0 | 25263.0 | 28415.0 | 19.87 | 8.06 | 164 | 0 |
| `read-ring` (arm-park) | 401969 | 26.34 | 1374.0 | 2433.0 | 2915.0 | 19.96 | 8.02 | 59 | 1 |
| `read-ring` (busy-poll) | 404265 | 26.49 | 686.0 | 1215.0 | 1473.0 | 158.28 | 63.99 | 52 | 63 |
| `read-ring` (thread-per-core park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 392655 | 25.73 | 2167.0 | 2733.0 | 3331.0 | 7.78 | 3.05 | 53 | 1 |
| kernel baseline | 445056 | 29.17 | 16375.0 | 36415.0 | 40159.0 | 9.70 | 4.32 | 29 | 0 |

#### 2× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T174121Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | 401982 | 26.34 | 1390.0 | 2455.0 | 2943.0 | 20.25 | 8.14 | 124 | 1 |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | 347382 | 22.77 | 482.0 | 793.0 | 37887.0 | 9.30 | 3.23 | 98 | 82 |
| `credit-ring` | 392280 | 25.71 | 2217.0 | 2865.0 | 3683.0 | 8.44 | 3.31 | 126 | 0 |
| kernel baseline | 443471 | 29.06 | 54463.0 | 124223.0 | 161663.0 | 9.39 | 4.16 | 30 | 0 |

#### 4× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T201434Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | 167029 | 10.95 | 1459.0 | 2551.0 | 3161.0 | 45.51 | 7.60 | 87 | 3 |
| `read-ring` (busy-poll) | 313500 | 20.55 | 684.0 | 1933.0 | 2667.0 | 204.07 | 63.98 | 93 | 2 |
| `read-ring` (thread-per-core park) | 114075 | 7.48 | 2241.0 | 2345.0 | 2411.0 | 17.87 | 2.04 | 92 | 0 |
| `credit-ring` | 178646 | 11.71 | 1244.0 | 2319.0 | 2899.0 | 43.96 | 7.85 | 83 | 0 |
| kernel baseline | 204477 | 13.40 | 1119.0 | 1880.0 | 2313.0 | 44.69 | 9.14 | 47 | 0 |

#### 4× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T205959Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | 388688 | 25.47 | 2853.0 | 4899.0 | 5747.0 | 21.97 | 8.54 | 100 | 2 |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 4× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** echo · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 512 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (busy-poll) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (thread-per-core park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |


### Table B — concurrency grid per transport

#### send-recv

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **transport:** `send-recv` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T165522Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 270294 | 17.71 | 233.0 | 312.0 | 353.0 | 12.20 | 3.30 | 25 | 0 |
| 1× vCPU | 64 | 416367 | 27.29 | 9839.0 | 10671.0 | 10943.0 | 9.98 | 4.15 | 88 | 0 |
| 1× vCPU | 512 | 413041 | 27.07 | 76159.0 | 83007.0 | 85823.0 | 9.75 | 4.03 | 559 | 2 |
| 2× vCPU | 1 | 175909 | 11.53 | 694.0 | 1152.0 | 1415.0 | 42.76 | 7.52 | 38 | 0 |
| 2× vCPU | 64 | 405772 | 26.59 | 20111.0 | 25263.0 | 28415.0 | 19.87 | 8.06 | 164 | 0 |
| 2× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T170415Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 200083 | 13.11 | 292.0 | 523.0 | 648.0 | 26.57 | 5.32 | 31 | 0 |
| 1× vCPU | 64 | 405540 | 26.58 | 703.0 | 1148.0 | 1305.0 | 9.00 | 3.65 | 35 | 0 |
| 1× vCPU | 512 | 405978 | 26.61 | 704.0 | 1144.0 | 1302.0 | 9.27 | 3.76 | 74 | 0 |
| 2× vCPU | 1 | 167248 | 10.96 | 730.0 | 1220.0 | 1503.0 | 43.52 | 7.28 | 48 | 0 |
| 2× vCPU | 64 | 401969 | 26.34 | 1374.0 | 2433.0 | 2915.0 | 19.96 | 8.02 | 59 | 1 |
| 2× vCPU | 512 | 401982 | 26.34 | 1390.0 | 2455.0 | 2943.0 | 20.25 | 8.14 | 124 | 1 |
| 4× vCPU | 1 | 167029 | 10.95 | 1459.0 | 2551.0 | 3161.0 | 45.51 | 7.60 | 87 | 3 |
| 4× vCPU | 64 | 388688 | 25.47 | 2853.0 | 4899.0 | 5747.0 | 21.97 | 8.54 | 100 | 2 |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (busy-poll)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **transport:** `read-ring` (busy-poll) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T171345Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 230237 | 15.09 | 203.0 | 726.0 | 1064.0 | 277.89 | 63.98 | 34 | 2 |
| 1× vCPU | 64 | 404284 | 26.50 | 631.0 | 1076.0 | 1272.0 | 158.24 | 63.97 | 40 | 3 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 1 | 274637 | 18.00 | 369.0 | 1197.0 | 1716.0 | 232.95 | 63.98 | 54 | 0 |
| 2× vCPU | 64 | 404265 | 26.49 | 686.0 | 1215.0 | 1473.0 | 158.28 | 63.99 | 52 | 63 |
| 2× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | 313500 | 20.55 | 684.0 | 1933.0 | 2667.0 | 204.07 | 63.98 | 93 | 2 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (thread-per-core park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **transport:** `read-ring` (thread-per-core park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T172218Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 99867 | 6.54 | 625.0 | 666.0 | 790.0 | 22.95 | 2.29 | 33 | 1 |
| 1× vCPU | 64 | 330692 | 21.67 | 1067.0 | 6939.0 | 10863.0 | 8.94 | 2.96 | 37 | 0 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 1 | 107926 | 7.07 | 1144.0 | 1205.0 | 1296.0 | 19.75 | 2.13 | 52 | 4 |
| 2× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 512 | 347382 | 22.77 | 482.0 | 793.0 | 37887.0 | 9.30 | 3.23 | 98 | 82 |
| 4× vCPU | 1 | 114075 | 7.48 | 2241.0 | 2345.0 | 2411.0 | 17.87 | 2.04 | 92 | 0 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T173535Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 319239 | 20.92 | 195.0 | 320.0 | 401.0 | 9.03 | 2.88 | 31 | 0 |
| 1× vCPU | 64 | 396014 | 25.95 | 1214.0 | 1572.0 | 1748.0 | 7.62 | 3.02 | 35 | 0 |
| 1× vCPU | 512 | 396169 | 25.96 | 1125.0 | 1407.0 | 1516.0 | 6.99 | 2.77 | 73 | 0 |
| 2× vCPU | 1 | 193340 | 12.67 | 540.0 | 1044.0 | 1294.0 | 37.95 | 7.34 | 46 | 0 |
| 2× vCPU | 64 | 392655 | 25.73 | 2167.0 | 2733.0 | 3331.0 | 7.78 | 3.05 | 53 | 1 |
| 2× vCPU | 512 | 392280 | 25.71 | 2217.0 | 2865.0 | 3683.0 | 8.44 | 3.31 | 126 | 0 |
| 4× vCPU | 1 | 178646 | 11.71 | 1244.0 | 2319.0 | 2899.0 | 43.96 | 7.85 | 83 | 0 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 8 KiB · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T174430Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 257665 | 16.89 | 238.0 | 375.0 | 441.0 | 17.41 | 4.49 | 19 | 0 |
| 1× vCPU | 64 | 445947 | 29.23 | 6351.0 | 20543.0 | 24303.0 | 8.59 | 3.83 | 19 | 0 |
| 1× vCPU | 512 | 445029 | 29.17 | 25903.0 | 73087.0 | 90367.0 | 9.00 | 4.01 | 20 | 0 |
| 2× vCPU | 1 | 305599 | 20.03 | 391.0 | 669.0 | 841.0 | 19.77 | 6.04 | 29 | 0 |
| 2× vCPU | 64 | 445056 | 29.17 | 16375.0 | 36415.0 | 40159.0 | 9.70 | 4.32 | 29 | 0 |
| 2× vCPU | 512 | 443471 | 29.06 | 54463.0 | 124223.0 | 161663.0 | 9.39 | 4.16 | 30 | 0 |
| 4× vCPU | 1 | 204477 | 13.40 | 1119.0 | 1880.0 | 2313.0 | 44.69 | 9.14 | 47 | 0 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

