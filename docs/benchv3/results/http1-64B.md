## HTTP/1.1 — 64 B

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T235145Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 239646 | 263.0 | 377.0 | 433.0 | 16.29 | 3.90 | 75 | 0 |
| `read-ring` (arm-park) | 295987 | 207.0 | 338.0 | 403.0 | 18.22 | 5.39 | 42 | 0 |
| `read-ring` (busy-poll) | 194831 | 238.0 | 928.0 | 1358.0 | 328.41 | 63.98 | 39 | 0 |
| `read-ring` (thread-per-core park) | 178265 | 347.0 | 393.0 | 738.0 | 22.21 | 3.96 | 37 | 1 |
| `credit-ring` | 270948 | 226.0 | 391.0 | 462.0 | 20.35 | 5.51 | 42 | 0 |
| kernel baseline | 291335 | 208.0 | 346.0 | 419.0 | 22.93 | 6.68 | 29 | 0 |

#### 2× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **connections:** 2× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183540Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 309834 | 404.0 | 600.0 | 695.0 | 20.49 | 6.35 | 131 | 0 |
| `read-ring` (arm-park) | 365624 | 332.0 | 584.0 | 707.0 | 19.92 | 7.28 | 69 | 0 |
| `read-ring` (busy-poll) | 190441 | 345.0 | 1213.0 | 1734.0 | 335.93 | 63.98 | 47 | 43 |
| `read-ring` (thread-per-core park) | 197022 | 653.0 | 724.0 | 982.0 | 18.23 | 3.59 | 59 | 0 |
| `credit-ring` | 369680 | 327.0 | 607.0 | 735.0 | 23.46 | 8.67 | 71 | 0 |
| kernel baseline | 322674 | 375.0 | 671.0 | 804.0 | 26.13 | 8.43 | 47 | 0 |

#### 4× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **connections:** 4× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T211446Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 306827 | 830.0 | 1077.0 | 1204.0 | 18.05 | 5.54 | 246 | 0 |
| `read-ring` (arm-park) | 477484 | 514.0 | 879.0 | 1066.0 | 19.41 | 9.27 | 119 | 0 |
| `read-ring` (busy-poll) | 184288 | 710.0 | 1888.0 | 2571.0 | 347.04 | 63.95 | 75 | 105 |
| `read-ring` (thread-per-core park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | 380523 | 636.0 | 1181.0 | 1409.0 | 28.66 | 10.90 | 75 | 0 |


### Table B — concurrency grid per transport

#### send-recv

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **transport:** `send-recv` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T182234Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 239646 | 263.0 | 377.0 | 433.0 | 16.29 | 3.90 | 75 | 0 |
| 2× vCPU | 1 | 309834 | 404.0 | 600.0 | 695.0 | 20.49 | 6.35 | 131 | 0 |
| 4× vCPU | 1 | 306827 | 830.0 | 1077.0 | 1204.0 | 18.05 | 5.54 | 246 | 0 |

#### read-ring (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T182447Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 295987 | 207.0 | 338.0 | 403.0 | 18.22 | 5.39 | 42 | 0 |
| 2× vCPU | 1 | 365624 | 332.0 | 584.0 | 707.0 | 19.92 | 7.28 | 69 | 0 |
| 4× vCPU | 1 | 477484 | 514.0 | 879.0 | 1066.0 | 19.41 | 9.27 | 119 | 0 |

#### read-ring (busy-poll)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **transport:** `read-ring` (busy-poll) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T182716Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 194831 | 238.0 | 928.0 | 1358.0 | 328.41 | 63.98 | 39 | 0 |
| 2× vCPU | 1 | 190441 | 345.0 | 1213.0 | 1734.0 | 335.93 | 63.98 | 47 | 43 |
| 4× vCPU | 1 | 184288 | 710.0 | 1888.0 | 2571.0 | 347.04 | 63.95 | 75 | 105 |

#### read-ring (thread-per-core park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **transport:** `read-ring` (thread-per-core park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183209Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 178265 | 347.0 | 393.0 | 738.0 | 22.21 | 3.96 | 37 | 1 |
| 2× vCPU | 1 | 197022 | 653.0 | 724.0 | 982.0 | 18.23 | 3.59 | 59 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183540Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 270948 | 226.0 | 391.0 | 462.0 | 20.35 | 5.51 | 42 | 0 |
| 2× vCPU | 1 | 369680 | 327.0 | 607.0 | 735.0 | 23.46 | 8.67 | 71 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** HTTP/1.1 · **payload:** 64 B · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T183810Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 291335 | 208.0 | 346.0 | 419.0 | 22.93 | 6.68 | 29 | 0 |
| 2× vCPU | 1 | 322674 | 375.0 | 671.0 | 804.0 | 26.13 | 8.43 | 47 | 0 |
| 4× vCPU | 1 | 380523 | 636.0 | 1181.0 | 1409.0 | 28.66 | 10.90 | 75 | 0 |

