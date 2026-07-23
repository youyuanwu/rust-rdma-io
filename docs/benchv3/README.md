# rdma-io-bench v3 — fixed-workload benchmark framework

This directory documents **how the `rdma-io-bench` RDMA-vs-TCP benchmark is run as a fixed
workload grid, and how results are organized over time**. It is the framework glue tying the
benchmark binaries, the orchestration, and the curated result tables together — modeled on the
[tonic-h3 bench framework](https://github.com/youyuanwu/tonic-h3/blob/main/docs/bench/README.md).

> **v3 vs the original [`docs/bench`](../bench/README.md).** The original bench tree characterizes
> each transport by **hunting for its peak** (a bespoke, hand-tuned config per number). Bench v3
> instead runs a **small fixed grid** that every transport is measured at *identically*, so
> numbers are directly comparable and trivially reproducible. `docs/bench` remains the historical
> peak-finding record; v3 is the going-forward, fixed-workload framework.

## Audience & purpose

You want to showcase RDMA throughput/latency/CPU-efficiency vs the kernel TCP baseline across
transports, scenarios, and VM SKUs — and to **collect those numbers repeatedly** as the code, the
SKUs, or the transports evolve. This framework standardizes the workload (a fixed grid), the run
procedure, and the result-table shapes so the dataset can grow without re-deciding what to measure.

## How the pieces fit

| Piece | Where | Role |
| ----- | ----- | ---- |
| Benchmark binaries (`rdma-bench-client` / `rdma-bench-server`) | [`tests/rdma-io-bench/`](../../tests/rdma-io-bench) | The actual load test across the transports and scenarios. |
| Orchestration (Ansible playbooks + a launcher) | [`tests/e2e/`](../../tests/e2e) (playbooks + in-repo `run_bench.sh`) | Deploys binaries/certs, launches server+client over the private link, fetches JSON results. bench v3 is **launcher-agnostic** — see [run-procedure › Launchers](run-procedure.md#launchers-pluggable). |
| Fixed-workload collection tooling | [`tests/benchv3/`](../../tests/benchv3) (`run_matrix.py` + `report.py`) | The primary in-repo launcher: expands the fixed grid, drives each coordinate through the orchestration, saves collision-proof results, and emits the result tables. |
| Fixed-workload docs (this tree) | `docs/benchv3/` | Defines the grid, the run procedure, and the result-table scaffolds. |

A run flows: **build** the release binaries → **deploy** them + TLS certs to the two VMs →
**run** each grid coordinate (server on `vm1`, client on `vm2`, `--report json`) → **collect** the
result JSON to the control node under collision-proof names → **generate** the result tables with
the in-repo [`tests/benchv3/`](../../tests/benchv3) runner + report (or transcribe by hand). See the
[run-procedure](run-procedure.md) for the exact commands.

## Design principle: fixed workload, repeated across SKUs

Every data point is a coordinate in the **fixed grid** defined once in the
[scenario matrix](scenario-matrix.md). That document is the source of truth for the thread,
connection, in-flight, payload, scenario, and transport-path axes; this README only describes how
the pieces fit together.

Because the coordinates are fixed and vCPU-relative, any two cells at the same coordinate are
directly comparable, and the **same grid** re-run on a new SKU or commit extends the dataset
without changing the framework.

## Adding data / a new SKU

To publish a new sweep:

1. Run the grid for the SKU per the [run-procedure](run-procedure.md) (`--threads = nproc`) — the
   in-repo [`tests/benchv3/run_matrix.py`](../../tests/benchv3/run_matrix.py) drives the whole grid and saves
   collision-proof results.
2. Generate the table blocks with [`tests/benchv3/report.py`](../../tests/benchv3/report.py) (paste-ready
   Table A/B), or copy a blank block from the [results-template](results-template.md) and fill each
   cell from the client's `--report json` output by hand.
3. State the fixed axes + provenance (SKU, vCPU, duration/warmup, git commit, date) in the table
   caption. Record any unsustainable coordinate as `n/a` / `fail`.

No framework edits are needed — the grid definition and table shapes are reused as-is.

## Quick links

| Doc | Purpose |
| --- | ------- |
| [scenario-matrix.md](scenario-matrix.md) | What/why we measure — transports, scenarios, the fixed grid (single source of truth) |
| [results-template.md](results-template.md) | Reusable blank result tables + how to fill a cell |
| [run-procedure.md](run-procedure.md) | How to run the grid with the real tooling + caveats |
| [../../tests/benchv3/README.md](../../tests/benchv3/README.md) | The in-repo grid runner + report generator (`run_matrix.py` / `report.py`) |
| [../bench/metrics.md](../bench/metrics.md) | Authoritative metric definitions (req/s, p50/p99 tail latencies, CPU/op, cores busy, peak RSS, Gbps; v3 also reports p95) |
