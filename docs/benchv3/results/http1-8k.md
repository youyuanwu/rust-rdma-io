## HTTP/1.1 — 8 KiB

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T235231Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 201791 | 13.22 | 310.0 | 459.0 | 534.0 | 26.43 | 5.33 | 79 | 0 |
| `read-ring` (arm-park) | 221082 | 14.49 | 277.0 | 474.0 | 576.0 | 32.63 | 7.21 | 50 | 0 |
| `read-ring` (busy-poll) | 182574 | 11.97 | 261.0 | 970.0 | 1423.0 | 350.20 | 63.94 | 46 | 0 |
| `read-ring` (thread-per-core park) | 113936 | 7.47 | 214.0 | 1946.0 | 3195.0 | 37.46 | 4.27 | 42 | 3 |
| `credit-ring` | 242306 | 15.88 | 250.0 | 440.0 | 530.0 | 33.08 | 8.02 | 48 | 0 |
| kernel baseline | 226327 | 14.83 | 268.0 | 440.0 | 532.0 | 38.86 | 8.79 | 37 | 0 |

#### 2× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183629Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 250510 | 16.42 | 496.0 | 775.0 | 909.0 | 28.83 | 7.22 | 141 | 0 |
| `read-ring` (arm-park) | 267986 | 17.56 | 451.0 | 832.0 | 1014.0 | 36.84 | 9.87 | 78 | 0 |
| `read-ring` (busy-poll) | 174893 | 11.46 | 452.0 | 1507.0 | 2189.0 | 365.78 | 63.97 | 62 | 28 |
| `read-ring` (thread-per-core park) | 116205 | 7.62 | 603.0 | 3529.0 | 4347.0 | 37.86 | 4.40 | 64 | 1 |
| `credit-ring` | 304366 | 19.95 | 396.0 | 744.0 | 903.0 | 33.44 | 10.18 | 77 | 0 |
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
| 1× vCPU | 1 | 221082 | 14.49 | 277.0 | 474.0 | 576.0 | 32.63 | 7.21 | 50 | 0 |
| 2× vCPU | 1 | 267986 | 17.56 | 451.0 | 832.0 | 1014.0 | 36.84 | 9.87 | 78 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (busy-poll)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `read-ring` (busy-poll) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T182924Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 182574 | 11.97 | 261.0 | 970.0 | 1423.0 | 350.20 | 63.94 | 46 | 0 |
| 2× vCPU | 1 | 174893 | 11.46 | 452.0 | 1507.0 | 2189.0 | 365.78 | 63.97 | 62 | 28 |
| 4× vCPU | 1 | 170008 | 11.14 | 1325.0 | 3233.0 | 4467.0 | 376.12 | 63.94 | 119 | 1 |

#### read-ring (thread-per-core park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `read-ring` (thread-per-core park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183255Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 113936 | 7.47 | 214.0 | 1946.0 | 3195.0 | 37.46 | 4.27 | 42 | 3 |
| 2× vCPU | 1 | 116205 | 7.62 | 603.0 | 3529.0 | 4347.0 | 37.86 | 4.40 | 64 | 1 |
| 4× vCPU | 1 | 120670 | 7.91 | 236.0 | 2305.0 | 3719.0 | 38.50 | 4.65 | 61 | 177 |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183629Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 242306 | 15.88 | 250.0 | 440.0 | 530.0 | 33.08 | 8.02 | 48 | 0 |
| 2× vCPU | 1 | 304366 | 19.95 | 396.0 | 744.0 | 903.0 | 33.44 | 10.18 | 77 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 8 KiB · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183855Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 226327 | 14.83 | 268.0 | 440.0 | 532.0 | 38.86 | 8.79 | 37 | 0 |
| 2× vCPU | 1 | 245679 | 16.10 | 491.0 | 875.0 | 1057.0 | 44.46 | 10.92 | 59 | 0 |
| 4× vCPU | 1 | 298129 | 19.54 | 799.0 | 1541.0 | 1852.0 | 45.48 | 13.56 | 91 | 0 |

