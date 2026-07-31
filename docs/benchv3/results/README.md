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

### Open-loop offered-load boards (1× vCPU)

The [loaded-latency and matched-throughput scenarios](../scenario-matrix.md#offered-load-scenarios-open-loop)
run a fixed sub-saturation offered rate instead of saturation (git commit `2bbfd3d`):

| Scenario | Loaded tail-latency | Matched throughput |
|---|---|---|
| echo · 64 B | [loaded-latency-echo-64B.md](loaded-latency-echo-64B.md) | [matched-throughput-echo-64B.md](matched-throughput-echo-64B.md) |
| echo · 8 KiB | [loaded-latency-echo-8k.md](loaded-latency-echo-8k.md) | [matched-throughput-echo-8k.md](matched-throughput-echo-8k.md) |
| HTTP/1.1 · 64 B | [loaded-latency-http1-64B.md](loaded-latency-http1-64B.md) | [matched-throughput-http1-64B.md](matched-throughput-http1-64B.md) |
| HTTP/1.1 · 8 KiB | [loaded-latency-http1-8k.md](loaded-latency-http1-8k.md) | [matched-throughput-http1-8k.md](matched-throughput-http1-8k.md) |

**Matched-throughput CPU sweep (deep pipeline):**
[matched-cpu-sweep-echo-64B.md](matched-cpu-sweep-echo-64B.md) — read-ring vs kernel TCP at 1M / 2M /
3M req/s (64 conn / in-flight 512). Shows the iso-throughput CPU-efficiency gap growing with load
(read-ring −13% cores at 1M → ~47% / nearly 2× at 3M).

**What they show.** At a **matched 250k req/s** echo 64 B load, the RDMA arm-park transports hold p50
≈ 600–740 µs vs the kernel baseline's ≈ 920 µs, at comparable-or-lower CPU/op; read-ring **busy-poll**
gives the lowest p50 (≈ 500 µs) but pins all 64 cores. In the **loaded-latency** sweeps,
`send-recv` / `read-ring` (arm-park) track the target cleanly up to ~2M req/s (64 B) with a flat
~400–500 µs p50, while `credit-ring` and the **thread-per-core park** topology reach a ceiling
(achieved falls below target and the tail blows up). At **8 KiB** the regime is bandwidth-bound:
`send-recv` and `credit-ring` stay clean at 150k req/s (≈ 9.8 Gbps) with `credit-ring` the most
CPU-efficient (~7.6 µs/op vs the kernel baseline's ~12.5 µs/op), while the ring completion topologies
hit their ceiling sooner (tail blowup / `n/a`). HTTP/1.1's rate is bounded by `connections / RTT`;
its read-ring **busy-poll** path breaks at higher rates and the read-ring **arm-park** path wedges
RDMA-CM at some rates (`n/a`) — the same MANA ring-CM flakiness seen in the closed-loop grid.
`errors > 0` cells remain suspect (see below).

## Coverage & caveats (read before citing)

This is a **partial dataset**, not the full grid. Coverage is uneven because of real
properties of the Azure MANA NIC and the benchmark clients — the empty (`n/a`) cells are documented
outcomes, not missing work. The fan-out axis is `{1×, 2×, 4×}` vCPU (64 / 128 / 256 connections);
every tier was collected with the reboot-between-sweeps cadence (`run_matrix.py --reboot-between`).

- **1× vCPU (64 conn) and 2× vCPU (128 conn): well covered** (63/72 and 60/72). echo is 67/72
  across these two tiers and HTTP/1.1 is essentially complete. gRPC is partial at both tiers (see
  below). The five remaining echo `n/a` cells at ≤ 2× are **not** transient misses —
  they hit two hard limits (see finding 5): four **busy-poll in-flight-512** cells exceed the
  device CQ depth, and **`send-recv` 128 conn × in-flight 512** wedges RDMA-CM setup on every
  attempt (including repeated fresh-NIC retries).
- **4× vCPU (256 conn): partially collected (30/72), and the split is informative.** Most of the
  **round-trip** (in-flight 1) and **moderate-pipeline** (in-flight 64) coordinates collect at 256
  connections — including the kernel-TCP baselines and the `send-recv` / `read-ring (arm-park)`
  paths. Two failure classes remain `n/a`: (a) the **deep-pipeline** coordinates (in-flight 512) and
  `send-recv` at deep concurrency, which overwhelm RDMA-CM setup; and (b) a handful of
  **`credit-ring` / thread-per-core-park** cells that wedge the CM handshake (`Rejected` /
  `Protocol error (os 71)` / 299 s timeout) at 256 connections **even at in-flight 1**, and did not
  recover across repeated fresh-NIC retries. These are NIC **setup**/flow-control properties, not
  data-path throughput ceilings. See the
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

4. **8 KiB ring cells use scenario-aware message sizing.** On the ring transports the RDMA message
   is `payload + framing`, so the 8 KiB rows set `--ring-max-msg` to **8192** for echo (raw
   payload) but **9216** for gRPC / HTTP-1.1 (payload + protobuf/gRPC/HTTP-2 or HTTP headers + TLS,
   ~8215 B on the wire). A flat 8192 would silently fragment the framed protocols' 8 KiB messages.
   The grid encodes this automatically; see the
   [scenario matrix](../scenario-matrix.md#payload).

5. **The read-ring busy-poll pool cannot reach in-flight 512 on this SKU — a hard device limit.**
   The busy-poll executor sizes a shared completion queue from `max_in_flight` (auto-derived as
   `in_flight * 2`), and at in-flight 512 it needs a CQ depth of ~2054 send / ~2052 recv, which
   exceeds the MANA device's `max_cqe = 2048`. The client rejects it immediately
   (`InvalidArg("busy pool: … shared CQ depth send=2054/recv=2052 > device max_cqe=2048")`), so
   those four echo cells (busy-poll · in-flight 512 · 1×/2× · 64 B/8 KiB) are a permanent `n/a` on
   this SKU — not a wedge and not fixable by rebooting. Separately, `send-recv` at 128 conn ×
   in-flight 512 (65 536 outstanding requests) wedges RDMA-CM setup (`os error 71`) on every
   attempt, including repeated fresh-NIC retries, and is also `n/a`.

### `errors > 0` cells are suspect

Per the [metric definitions](../results-template.md#how-to-read--fill-a-cell), **a cell with
non-zero `errors` is not a clean data point.** Several cells here carry small error counts (often
1–3, likely connection-teardown races, plus a handful of busy-poll / park cells in the tens–low
hundreds) and one carries a large count — **gRPC `credit-ring` 8 KiB / in-flight 64 (~24 k
errors)**, an unsustainable coordinate for that path at any ring size. Treat any row with a non-zero
`errors` column as **illustrative only** and re-run it in isolation before citing the number.

## Headline (echo, the most complete scenario)

At the `1× vCPU · in-flight 64 · 64 B` coordinate, the RDMA paths reach comparable or higher
throughput than the kernel-TCP baseline at a **fraction of the CPU per operation** — the CPU
efficiency being the clean story (see [echo-64B.md](echo-64B.md)). The busy-poll topology trades
full CPU occupancy (`cores ≈ 64`) for peak throughput/latency, exactly as designed.
