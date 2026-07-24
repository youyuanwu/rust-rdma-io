## HTTP/1.1 — 8 KiB

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T235231Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 201791 | 13.22 | 310.0 | 459.0 | 534.0 | 26.43 | 5.33 | 79 | 0 |
| `read-ring` (arm-park) | 242408 | 15.89 | 250.0 | 423.0 | 516.0 | 29.02 | 7.03 | 49 | 0 |
| `read-ring` (busy-poll) | 185473 | 12.16 | 257.0 | 949.0 | 1386.0 | 345.02 | 63.99 | 46 | 0 |
| `read-ring` (thread-per-core park) | 132471 | 8.68 | 424.0 | 819.0 | 912.0 | 31.21 | 4.13 | 40 | 0 |
| `credit-ring` | 246549 | 16.16 | 246.0 | 432.0 | 526.0 | 32.22 | 7.94 | 50 | 0 |
| kernel baseline | 226327 | 14.83 | 268.0 | 440.0 | 532.0 | 38.86 | 8.79 | 37 | 0 |

#### 2× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183629Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 250510 | 16.42 | 496.0 | 775.0 | 909.0 | 28.83 | 7.22 | 141 | 0 |
| `read-ring` (arm-park) | 276542 | 18.12 | 437.0 | 777.0 | 946.0 | 30.17 | 8.34 | 77 | 0 |
| `read-ring` (busy-poll) | 185826 | 12.18 | 379.0 | 1277.0 | 1828.0 | 344.36 | 63.99 | 51 | 38 |
| `read-ring` (thread-per-core park) | 141089 | 9.25 | 317.0 | 581.0 | 688.0 | 31.70 | 4.47 | 38 | 80 |
| `credit-ring` | 309581 | 20.29 | 389.0 | 728.0 | 892.0 | 33.74 | 10.45 | 77 | 0 |
| kernel baseline | 245679 | 16.10 | 491.0 | 875.0 | 1057.0 | 44.46 | 10.92 | 59 | 0 |

#### 4× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T211531Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 313271 | 20.53 | 795.0 | 1236.0 | 1454.0 | 33.19 | 10.40 | 263 | 0 |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (busy-poll) | 170008 | 11.14 | 1325.0 | 3233.0 | 4467.0 | 376.12 | 63.94 | 119 | 1 |
| `read-ring` (thread-per-core park) | 120670 | 7.91 | 236.0 | 2305.0 | 3719.0 | 38.50 | 4.65 | 61 | 177 |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | 298129 | 19.54 | 799.0 | 1541.0 | 1852.0 | 45.48 | 13.56 | 91 | 0 |


### Table B — concurrency grid per transport

#### send-recv

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `send-recv` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T182322Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 201791 | 13.22 | 310.0 | 459.0 | 534.0 | 26.43 | 5.33 | 79 | 0 |
| 2× vCPU | 1 | 250510 | 16.42 | 496.0 | 775.0 | 909.0 | 28.83 | 7.22 | 141 | 0 |
| 4× vCPU | 1 | 313271 | 20.53 | 795.0 | 1236.0 | 1454.0 | 33.19 | 10.40 | 263 | 0 |

#### read-ring (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T182536Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 242408 | 15.89 | 250.0 | 423.0 | 516.0 | 29.02 | 7.03 | 49 | 0 |
| 2× vCPU | 1 | 276542 | 18.12 | 437.0 | 777.0 | 946.0 | 30.17 | 8.34 | 77 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (busy-poll)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `read-ring` (busy-poll) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T182924Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 185473 | 12.16 | 257.0 | 949.0 | 1386.0 | 345.02 | 63.99 | 46 | 0 |
| 2× vCPU | 1 | 185826 | 12.18 | 379.0 | 1277.0 | 1828.0 | 344.36 | 63.99 | 51 | 38 |
| 4× vCPU | 1 | 170008 | 11.14 | 1325.0 | 3233.0 | 4467.0 | 376.12 | 63.94 | 119 | 1 |

#### read-ring (thread-per-core park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `read-ring` (thread-per-core park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183255Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 132471 | 8.68 | 424.0 | 819.0 | 912.0 | 31.21 | 4.13 | 40 | 0 |
| 2× vCPU | 1 | 141089 | 9.25 | 317.0 | 581.0 | 688.0 | 31.70 | 4.47 | 38 | 80 |
| 4× vCPU | 1 | 120670 | 7.91 | 236.0 | 2305.0 | 3719.0 | 38.50 | 4.65 | 61 | 177 |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183629Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 246549 | 16.16 | 246.0 | 432.0 | 526.0 | 32.22 | 7.94 | 50 | 0 |
| 2× vCPU | 1 | 309581 | 20.29 | 389.0 | 728.0 | 892.0 | 33.74 | 10.45 | 77 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183855Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 226327 | 14.83 | 268.0 | 440.0 | 532.0 | 38.86 | 8.79 | 37 | 0 |
| 2× vCPU | 1 | 245679 | 16.10 | 491.0 | 875.0 | 1057.0 | 44.46 | 10.92 | 59 | 0 |
| 4× vCPU | 1 | 298129 | 19.54 | 799.0 | 1541.0 | 1852.0 | 45.48 | 13.56 | 91 | 0 |

