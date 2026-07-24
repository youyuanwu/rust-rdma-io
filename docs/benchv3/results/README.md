# Bench v3 results — collected data

Real bench v3 numbers collected with the in-repo [`tests/benchv3/`](../../../tests/benchv3) runner +
report generator, on a two-VM Azure RoCEv2 setup. Each file below is generated verbatim by
`report.py --all` (one file per scenario × payload) and lightly annotated; the table shapes and
metric definitions are fixed by the [scenario matrix](../scenario-matrix.md) and
[results template](../results-template.md#how-to-read--fill-a-cell).

> Raw run artifacts are **not** committed (see the [run procedure](../run-procedure.md)); only these
> curated tables are.

## Environment

| | |
|---|---|
| **SKU** | `Standard_E64bs_v6` (uksouth), 64 vCPU |
| **NIC** | Azure MANA RoCEv2 |
| **threads** | 64 (= vCPU) on both peers |
| **duration / warmup** | 10 s / 3 s |
| **git commit** | `a3d99d0` (built + deployed to both VMs) |
| **date** | 2026-07-23 → 2026-07-24 (1× first pass; 2× / 4× tiers added later) |

## Files

| Scenario | 64 B | 8 KiB |
|---|---|---|
| echo | [echo-64B.md](echo-64B.md) | [echo-8k.md](echo-8k.md) |
| gRPC (rh2) | [grpc-64B.md](grpc-64B.md) | [grpc-8k.md](grpc-8k.md) |
| HTTP/1.1 (rh1) | [http1-64B.md](http1-64B.md) | [http1-8k.md](http1-8k.md) |

## Coverage & caveats (read before citing)

This is a **partial dataset**, not the full grid. Coverage is uneven because of real
properties of the Azure MANA NIC and the benchmark clients — the empty (`n/a`) cells are documented
outcomes, not missing work. The fan-out axis is `{1×, 2×, 4×}` vCPU (64 / 128 / 256 connections);
every tier was collected with the reboot-between-sweeps cadence (`run_matrix.py --reboot-between`).

- **1× vCPU (64 conn) and 2× vCPU (128 conn): well covered.** echo and HTTP/1.1 are essentially
  complete; the 2× sweep landed 58/72 coordinates. gRPC is partial at both tiers (see below).
- **4× vCPU (256 conn): partial, and the split is informative.** The **round-trip regime**
  (in-flight 1) and **moderate-pipeline ring paths** (in-flight 64) collect cleanly at 256
  connections. The **deep-pipeline** coordinates (in-flight 512) and the **`send-recv`** path
  instead wedge or hit the ansible run timeout — the MANA RDMA-CM handshake stalls under the
  combined connection + outstanding-request pressure (`ibverbs Protocol error (os 71)`, `Rejected`,
  299 s timeouts). Those cells are `n/a`; it is a NIC **setup**/flow-control property, not a
  data-path throughput ceiling. See the
  [run procedure](../run-procedure.md#high-connections-4-vcpu).

### Findings worth calling out

1. **The MANA NIC wedges cumulatively, not just at high connection counts.** Running many RDMA-CM
   connect/teardown cycles back-to-back progressively wedges the NIC even at 1× (64 connections),
   so a clean sweep must reboot between transport-path groups (`--reboot-between`) regardless of
   connection count. This is consistent with the existing
   [methodology notes](../../bench/methodology.md#reboot-cadence-and-nic-wedges).

2. **gRPC-over-RDMA (`rh2`) is markedly more CM-sensitive than echo / HTTP/1.1.** Its ring and
   send-recv paths hit transient handshake rejects (`expected Established, got Rejected` /
   `ConnectError`) that the gRPC bench client does **not** retry (it gives up after a fixed number
   of connect attempts), where the async-CM code retries with a fresh CM ID. As a result several
   gRPC cells (across all tiers) could not be collected even with reboots and are shown as `n/a`.
   This is a genuine client-robustness gap, not measurement noise — see [grpc-64B.md](grpc-64B.md).

3. **At 4× (256 conn) the limit is outstanding-request pressure, not raw connection count.** The
   round-trip (in-flight 1) and moderate-pipeline (in-flight 64) coordinates collect cleanly at 256
   connections, but the deep-pipeline (in-flight 512) coordinates and the `send-recv` path stall the
   RDMA-CM handshake and hit the run timeout. So the 4× gaps are concentrated in the deepest-pipeline
   cells, not spread uniformly — the fan-out itself is sustainable; the fan-out **×** pipeline-depth
   product is what tips the NIC into its setup-flakiness regime.

### `errors > 0` cells are suspect

Per the [metric definitions](../results-template.md#how-to-read--fill-a-cell), **a cell with
non-zero `errors` is not a clean data point.** Several cells here carry small error counts (often
1–3, likely connection-teardown races) and a few carry large counts — most notably **gRPC
`credit-ring` 8 KiB / in-flight 64 reported ~25 k errors** (an unsustainable coordinate for that
path) and some HTTP/1.1 busy-poll cells reported ~60 errors. Treat any row with a non-zero `errors`
column as **illustrative only** and re-run it in isolation before citing the number.

## Headline (echo, the most complete scenario)

At the `1× vCPU · in-flight 64 · 64 B` coordinate, the RDMA paths reach comparable or higher
throughput than the kernel-TCP baseline at a **fraction of the CPU per operation** — the CPU
efficiency being the clean story (see [echo-64B.md](echo-64B.md)). The busy-poll topology trades
full CPU occupancy (`cores ≈ 64`) for peak throughput/latency, exactly as designed.
