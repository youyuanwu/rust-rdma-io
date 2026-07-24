## gRPC — 64 B

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 64 B · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T232651Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 185415 | 330.0 | 547.0 | 645.0 | 65.02 | 12.06 | 81 | 0 |
| `read-ring` (arm-park) | 238409 | 252.0 | 428.0 | 521.0 | 64.70 | 15.43 | 51 | 0 |
| `credit-ring` | 255181 | 235.0 | 404.0 | 501.0 | 72.86 | 18.59 | 51 | 0 |
| kernel baseline | 211936 | 288.0 | 448.0 | 539.0 | 84.41 | 17.89 | 37 | 0 |

#### 1× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 64 B · **connections:** 1× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T232824Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 746123 | 5159.0 | 8671.0 | 10703.0 | 63.25 | 47.19 | 1221 | 0 |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 742875 | 5163.0 | 8751.0 | 10815.0 | 62.34 | 46.31 | 706 | 0 |
| kernel baseline | 688640 | 5567.0 | 9495.0 | 11695.0 | 66.73 | 45.96 | 689 | 0 |

#### 1× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 64 B · **connections:** 1× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T225210Z`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | 131398 | 194559.0 | 318463.0 | 382207.0 | 269.61 | 35.43 | 5039 | 0 |

#### 4× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 64 B · **connections:** 4× vCPU · **in-flight:** 1 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 4× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 64 B · **connections:** 4× vCPU · **in-flight:** 64 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 4× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 64 B · **connections:** 4× vCPU · **in-flight:** 512 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 16× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 64 B · **connections:** 16× vCPU · **in-flight:** 1 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 16× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 64 B · **connections:** 16× vCPU · **in-flight:** 64 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 16× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 64 B · **connections:** 16× vCPU · **in-flight:** 512 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |


### Table B — concurrency grid per transport

#### send-recv

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 64 B · **transport:** `send-recv` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T223053Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 185415 | 330.0 | 547.0 | 645.0 | 65.02 | 12.06 | 81 | 0 |
| 1× vCPU | 64 | 746123 | 5159.0 | 8671.0 | 10703.0 | 63.25 | 47.19 | 1221 | 0 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 64 B · **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T223525Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 238409 | 252.0 | 428.0 | 521.0 | 64.70 | 15.43 | 51 | 0 |
| 1× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 64 B · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T232651Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 255181 | 235.0 | 404.0 | 501.0 | 72.86 | 18.59 | 51 | 0 |
| 1× vCPU | 64 | 742875 | 5163.0 | 8751.0 | 10815.0 | 62.34 | 46.31 | 706 | 0 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 64 B · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T224911Z`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 211936 | 288.0 | 448.0 | 539.0 | 84.41 | 17.89 | 37 | 0 |
| 1× vCPU | 64 | 688640 | 5567.0 | 9495.0 | 11695.0 | 66.73 | 45.96 | 689 | 0 |
| 1× vCPU | 512 | 131398 | 194559.0 | 318463.0 | 382207.0 | 269.61 | 35.43 | 5039 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

