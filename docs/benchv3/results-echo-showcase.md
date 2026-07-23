# Results — echo scenario (showcase)

A first real collection of the bench v3 grid for the **echo** scenario, produced with the in-repo
[`tests/benchv3/`](../../tests/benchv3) runner + report generator. This is a **representative
showcase**, not the full grid — see [Coverage](#coverage) below. It demonstrates the
RDMA-vs-kernel-TCP comparison the framework is built to standardize.

> Numbers are curated from the raw `--report json` output by `report.py`; raw run artifacts are not
> committed (see the [run procedure](run-procedure.md)). Metric definitions are in the
> [results template](results-template.md#how-to-read--fill-a-cell).

## Table A — kernel-baseline-vs-RDMA at one coordinate

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B ·
> **connections:** 1× vCPU (64) · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s ·
> **git commit:** `7259b44` · **date:** 2026-07-23

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv`    | 4,298,273          | 176      | 554      | 1192     | 1.47        | 6.30  | 25            | 0      |
| `read-ring` (arm-park) | 4,593,836  | 221      | 861      | 1601     | 1.26        | 5.77  | 35            | 0      |
| `read-ring` (busy-poll)² | 11,552,047 | 332    | 459      | 679      | 5.54        | 63.98 | 41            | 1 ⚠️   |
| `read-ring` (thread-per-core park)² | 4,024,414 | 132 | 589   | 1048     | 1.69        | 6.79  | 37            | 0      |
| `credit-ring`¹ | 976,176            | 4183     | 4483     | 4607     | 3.09        | 3.02  | 35            | 0      |
| kernel baseline | 6,344,755         | 288      | 824      | 1478     | 5.69        | 36.11 | 16            | 0      |

¹ `credit-ring` sustains this coordinate but at much lower throughput / higher latency than the
other paths — an expected characteristic of the credit-based ring at this depth, not a failure.
² `read-ring` busy-poll / thread-per-core park are read-ring completion topologies (`echo-busy` /
`echo-park`), echo & HTTP/1.1 only.

⚠️ **The busy-poll cell reported `errors: 1`, so it is not a clean data point** — a non-zero
`errors` value must be investigated before the number is trusted (see the
[metric definitions](results-template.md#how-to-read--fill-a-cell)). It is shown here only to
illustrate the busy-poll behavior: it spins every core (`cores ≈ 64`) to push ~11.5 M req/s, i.e. it
trades full CPU occupancy for throughput/latency. Re-run it in isolation before citing the number.

**Reading it:** at this coordinate the RDMA `send-recv` and `read-ring (arm-park)` paths deliver
~4.3–4.6 M req/s at markedly lower CPU/op (1.3–1.5 µs) than the kernel TCP baseline (6.3 M req/s but
5.7 µs/op and ~36 cores busy). The clean comparison is CPU-efficiency: the RDMA paths reach
comparable throughput using a fraction of the CPU.

## Table B — concurrency grid for `read-ring` (arm-park)

> **SKU:** `Standard_E64bs_v6` (uksouth) · **vCPU:** 64 · **scenario:** echo · **payload:** 64 B ·
> **transport:** `read-ring` (arm-park) · **duration/warmup:** 10 s / 3 s · **git commit:** `7259b44` ·
> **date:** 2026-07-23

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU     | 1         | 273,040            | 231      | 314      | 355      | 10.57       | 2.89  | 31            | 0      |
| 1× vCPU     | 64        | 4,593,836          | 221      | 861      | 1601     | 1.26        | 5.77  | 35            | 0      |
| 1× vCPU     | 512       | 5,198,375          | 440      | 1610     | 2519     | 0.94        | 4.88  | 70            | 0      |
| 4× vCPU     | 1         | 178,560            | 1388     | 2405     | 2989     | 42.61       | 7.61  | 88            | 0      |
| 4× vCPU     | 64        | 4,439,692          | 200      | 632      | 1455     | 1.19        | 5.26  | 97            | 1 ⚠️   |
| 4× vCPU     | 512       | `n/a`              | `n/a`    | `n/a`    | `n/a`    | `n/a`       | `n/a` | `n/a`         | `n/a`  |
| 16× vCPU    | 1         | `n/a`              | `n/a`    | `n/a`    | `n/a`    | `n/a`       | `n/a` | `n/a`         | `n/a`  |
| 16× vCPU    | 64        | `n/a`              | `n/a`    | `n/a`    | `n/a`    | `n/a`       | `n/a` | `n/a`         | `n/a`  |
| 16× vCPU    | 512       | `n/a`              | `n/a`    | `n/a`    | `n/a`    | `n/a`       | `n/a` | `n/a`         | `n/a`  |

⚠️ The `4× / 64` cell reported `errors: 1` — treat as suspect and re-run before citing.

The `n/a` cells were **not collected** in this showcase: at 4× / 512 in-flight and all 16×
(1024-connection) coordinates the run hit the documented MANA RDMA-CM setup wedge
(`ibverbs Protocol error`), which is a NIC setup property rather than a data-path result — see the
[run procedure](run-procedure.md#reboot-cadence). Collecting those cells cleanly requires the
reboot-between-sweeps cadence (`run_matrix.py --reboot-between`) and a longer time budget.

## Coverage

This showcase collected two of the echo boards rather than the full grid:

- **Table A:** the `1× vCPU · in-flight 64 · 64 B` coordinate, all 6 transport paths (complete).
- **Table B:** the `read-ring (arm-park) · 64 B` concurrency grid (5 of 9 coordinates; deep-
  concurrency cells pending a reboot-cadence run).

The remaining echo coordinates (other in-flight/connection points, the 8 KiB payload, and the
gRPC / HTTP/1.1 scenarios) follow the same procedure and can be added to this file over time; the
grid definition and table shapes are fixed by the [scenario matrix](scenario-matrix.md) and
[results template](results-template.md).

## Reproducing

```
# from the control node, with the two RDMA VMs deployed (see run-procedure.md):
python3 tests/benchv3/run_matrix.py --vcpu 64 --scenario echo \
  --connections-mult 1 --in-flight 64 --payload 64 --duration 10 --warmup 3
python3 tests/benchv3/report.py --table a --scenario echo --payload 64 \
  --connections-mult 1 --in-flight 64
```
