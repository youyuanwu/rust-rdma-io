## gRPC — 8 KiB

### Table A — per-coordinate comparison

#### 1× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T224409Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 97512 | 6.39 | 645.0 | 902.0 | 1018.0 | 102.53 | 10.00 | 98 | 0 |
| `read-ring` (arm-park) | 207753 | 13.62 | 293.0 | 457.0 | 560.0 | 94.41 | 19.61 | 57 | 0 |
| `credit-ring` | 228268 | 14.96 | 266.0 | 430.0 | 525.0 | 98.48 | 22.48 | 57 | 0 |
| kernel baseline | 170623 | 11.18 | 360.0 | 538.0 | 651.0 | 127.26 | 21.71 | 44 | 0 |

#### 1× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260723T232912Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 10679 | 0.70 | 4591.0 | 1316863.0 | 1970175.0 | 199.08 | 2.13 | 1306 | 0 |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 7 | 0.00 | 5419007.0 | 10969087.0 | 10969087.0 | 51666.67 | 0.37 | 814 | 24057 |
| kernel baseline | 313351 | 20.54 | 12047.0 | 22527.0 | 28239.0 | 128.79 | 40.36 | 827 | 0 |

#### 1× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 1× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T221955Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 0 | 0.00 | 0.0 | 0.0 | 0.0 |  | 0.17 | 966 | 0 |
| kernel baseline | 147118 | 9.64 | 169215.0 | 292351.0 | 422911.0 | 210.05 | 30.90 | 5689 | 0 |

#### 2× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T181033Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | 108033 | 7.08 | 1168.0 | 1595.0 | 1810.0 | 118.25 | 12.78 | 176 | 0 |
| `read-ring` (arm-park) | 242879 | 15.92 | 495.0 | 873.0 | 1079.0 | 101.50 | 24.65 | 88 | 0 |
| `credit-ring` | 280796 | 18.40 | 423.0 | 774.0 | 973.0 | 107.99 | 30.32 | 87 | 0 |
| kernel baseline | 210307 | 13.78 | 569.0 | 986.0 | 1220.0 | 134.22 | 28.23 | 67 | 0 |

#### 2× vCPU · in-flight 64

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T181213Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 0 | 0.00 | 0.0 | 0.0 | 0.0 |  | 0.07 | 277 | 0 |
| kernel baseline | 330895 | 21.69 | 22271.0 | 41439.0 | 52991.0 | 128.94 | 42.67 | 1558 | 0 |

#### 2× vCPU · in-flight 512

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 2× vCPU · **in-flight:** 512 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T181452Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 0 | 0.00 | 0.0 | 0.0 | 0.0 |  | 0.04 | 1862 | 0 |
| kernel baseline | 87958 | 5.76 | 420607.0 | 827391.0 | 1051647.0 | 278.28 | 24.48 | 11142 | 0 |

#### 4× vCPU · in-flight 1

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **connections:** 4× vCPU · **in-flight:** 1 · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T203711Z`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `read-ring` (arm-park) | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| `credit-ring` | 292913 | 19.20 | 806.0 | 1582.0 | 1993.0 | 120.21 | 35.21 | 144 | 0 |
| kernel baseline | 256865 | 16.83 | 909.0 | 1741.0 | 2201.0 | 147.46 | 37.88 | 104 | 0 |

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


### Table B — concurrency grid per transport

#### send-recv

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** `send-recv` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T175153Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 97512 | 6.39 | 645.0 | 902.0 | 1018.0 | 102.53 | 10.00 | 98 | 0 |
| 1× vCPU | 64 | 10679 | 0.70 | 4591.0 | 1316863.0 | 1970175.0 | 199.08 | 2.13 | 1306 | 0 |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 1 | 108033 | 7.08 | 1168.0 | 1595.0 | 1810.0 | 118.25 | 12.78 | 176 | 0 |
| 2× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### read-ring (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T175949Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 207753 | 13.62 | 293.0 | 457.0 | 560.0 | 94.41 | 19.61 | 57 | 0 |
| 1× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 1× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 1 | 242879 | 15.92 | 495.0 | 873.0 | 1079.0 | 101.50 | 24.65 | 88 | 0 |
| 2× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 2× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 1 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### credit-ring

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** `credit-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T181033Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 228268 | 14.96 | 266.0 | 430.0 | 525.0 | 98.48 | 22.48 | 57 | 0 |
| 1× vCPU | 64 | 7 | 0.00 | 5419007.0 | 10969087.0 | 10969087.0 | 51666.67 | 0.37 | 814 | 24057 |
| 1× vCPU | 512 | 0 | 0.00 | 0.0 | 0.0 | 0.0 |  | 0.17 | 966 | 0 |
| 2× vCPU | 1 | 280796 | 18.40 | 423.0 | 774.0 | 973.0 | 107.99 | 30.32 | 87 | 0 |
| 2× vCPU | 64 | 0 | 0.00 | 0.0 | 0.0 | 0.0 |  | 0.07 | 277 | 0 |
| 2× vCPU | 512 | 0 | 0.00 | 0.0 | 0.0 | 0.0 |  | 0.04 | 1862 | 0 |
| 4× vCPU | 1 | 292913 | 19.20 | 806.0 | 1582.0 | 1993.0 | 120.21 | 35.21 | 144 | 0 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

#### kernel baseline

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** gRPC · **payload:** 8 KiB · **transport:** kernel baseline · **duration/warmup:** 10 s / 3 s · **git commit:** `a3d99d0` · **date:** `20260724T181810Z`

| connections | in-flight | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU | 1 | 170623 | 11.18 | 360.0 | 538.0 | 651.0 | 127.26 | 21.71 | 44 | 0 |
| 1× vCPU | 64 | 313351 | 20.54 | 12047.0 | 22527.0 | 28239.0 | 128.79 | 40.36 | 827 | 0 |
| 1× vCPU | 512 | 147118 | 9.64 | 169215.0 | 292351.0 | 422911.0 | 210.05 | 30.90 | 5689 | 0 |
| 2× vCPU | 1 | 210307 | 13.78 | 569.0 | 986.0 | 1220.0 | 134.22 | 28.23 | 67 | 0 |
| 2× vCPU | 64 | 330895 | 21.69 | 22271.0 | 41439.0 | 52991.0 | 128.94 | 42.67 | 1558 | 0 |
| 2× vCPU | 512 | 87958 | 5.76 | 420607.0 | 827391.0 | 1051647.0 | 278.28 | 24.48 | 11142 | 0 |
| 4× vCPU | 1 | 256865 | 16.83 | 909.0 | 1741.0 | 2201.0 | 147.46 | 37.88 | 104 | 0 |
| 4× vCPU | 64 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |
| 4× vCPU | 512 | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` | `n/a` |

