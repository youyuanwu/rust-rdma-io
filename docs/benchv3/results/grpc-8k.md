## gRPC — 8 KiB

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T224409Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 97512 | 6.39 | 645.0 | 902.0 | 1018.0 | 102.53 | 10.00 | 98 | 0 |
| `read-ring` (arm-park) | 207150 | 13.58 | 295.0 | 458.0 | 564.0 | 94.42 | 19.56 | 56 | 0 |
| `credit-ring` | 227982 | 14.94 | 267.0 | 424.0 | 517.0 | 103.05 | 23.49 | 56 | 0 |
| kernel baseline | 170623 | 11.18 | 360.0 | 538.0 | 651.0 | 127.26 | 21.71 | 44 | 0 |

#### 1× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T232912Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 10679 | 0.70 | 4591.0 | 1316863.0 | 1970175.0 | 199.08 | 2.13 | 1306 | 0 |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 4 | 0.00 | 10829823.0 | 10838015.0 | 10838015.0 | 94523.81 | 0.40 | 812 | 25325 |
| kernel baseline | 313351 | 20.54 | 12047.0 | 22527.0 | 28239.0 | 128.79 | 40.36 | 827 | 0 |

#### 1× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T225256Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | 147118 | 9.64 | 169215.0 | 292351.0 | 422911.0 | 210.05 | 30.90 | 5689 | 0 |

#### 4× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 1 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 4× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 64 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 4× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 512 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 16× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 16× vCPU · **in-flight:** 1 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 16× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 16× vCPU · **in-flight:** 64 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### 16× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** `__` · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 16× vCPU · **in-flight:** 512 · **duration/warmup:** `__` s / `__` s · **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| kernel baseline | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |


### Table B — concurrency grid per transport

#### send-recv

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** `send-recv` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T223138Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 97512 | 6.39 | 645.0 | 902.0 | 1018.0 | 102.53 | 10.00 | 98 | 0 |
| 1× vCPU | 64 | 10679 | 0.70 | 4591.0 | 1316863.0 | 1970175.0 | 199.08 | 2.13 | 1306 | 0 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T223611Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 207150 | 13.58 | 295.0 | 458.0 | 564.0 | 94.42 | 19.56 | 56 | 0 |
| 1× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T224409Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 227982 | 14.94 | 267.0 | 424.0 | 517.0 | 103.05 | 23.49 | 56 | 0 |
| 1× vCPU | 64 | 4 | 0.00 | 10829823.0 | 10838015.0 | 10838015.0 | 94523.81 | 0.40 | 812 | 25325 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T224957Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 170623 | 11.18 | 360.0 | 538.0 | 651.0 | 127.26 | 21.71 | 44 | 0 |
| 1× vCPU | 64 | 313351 | 20.54 | 12047.0 | 22527.0 | 28239.0 | 128.79 | 40.36 | 827 | 0 |
| 1× vCPU | 512 | 147118 | 9.64 | 169215.0 | 292351.0 | 422911.0 | 210.05 | 30.90 | 5689 | 0 |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 16× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

