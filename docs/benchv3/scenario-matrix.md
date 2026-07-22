# Benchmark scenario matrix (bench v3)

This document defines **what** bench v3 measures and **why**. It is the single source of
truth for the **fixed-workload grid**; the other v3 docs
([results-template](results-template.md), [run-procedure](run-procedure.md)) reference this
grid rather than redefining it. For the framework overview start at the
[bench v3 README](README.md).

## The fixed-workload principle

The older [`docs/bench`](../bench/README.md) write-ups characterize each transport by
**hunting for its peak** — every number is measured at a bespoke, hand-tuned
`connections × in-flight × depth`. Bench v3 does the opposite: it runs a **small, fixed grid
of coordinates** that every transport is measured at *identically*, so numbers are directly
comparable (same coordinate across transports) and trivially reproducible (the grid is the
spec). There is **no peak-finding and no per-run tuning** — every table cell is a fixed
coordinate.

The grid is defined **relative to the machine's vCPU count**, so the same document applies to
any VM SKU: record the SKU's vCPU count and the multiples below resolve to concrete numbers.

A single data point is one `rdma-bench-client` invocation against one `rdma-bench-server`,
pinned to a `(scenario, transport, threads, connections, in-flight, payload)` coordinate, run
on a specific VM SKU. The executor budget (`--threads`) is **always the vCPU count** and is
**not** a load dimension — offered load is `connections × in-flight` (see
[`--threads` semantics](../bench/metrics.md)).

## The axes

### Transport paths

The three RDMA [`Transport`](../design/rdma-transport-layer.md) implementations plus a kernel
baseline. The baseline is a **path, not a uniform `--transport` value**: for `echo` it is
`--transport tcp`; for gRPC and HTTP/1.1 it is a distinct *mode* (`tcp` / `tcp1`).

| Path | Stack | Role |
|---|---|---|
| `send-recv` | Two-sided RDMA SEND/RECV, per-connection recv-buffer pool | RDMA transport under test |
| `read-ring` | One-sided RDMA READ ring (busy-poll capable) | RDMA transport under test |
| `credit-ring` | Credit-based ring with explicit credit round-trips | RDMA transport under test |
| **kernel baseline** | Raw TCP over the same application stack (`--transport tcp` for echo; mode `tcp`/`tcp1` for gRPC/HTTP-1.1) | The reference every RDMA path is compared against |

### Scenarios

Each scenario adds or removes application layers so a number maps to a specific part of the
stack. See the shared scenario docs: [echo](../bench/scenarios/echo.md) ·
[gRPC](../bench/scenarios/grpc.md) · [HTTP/1.1](../bench/scenarios/h1.md).

| Scenario | RDMA mode | Kernel baseline | What it isolates |
|---|---|---|---|
| **echo** | `--mode echo --transport <rdma>` | `--mode echo --transport tcp` | Raw `Transport` data path — no gRPC/TLS/HTTP |
| **gRPC** | `--mode rh2 --transport <rdma>` | `--mode tcp` | tonic over TLS 1.3 + HTTP/2 + protobuf + hyper |
| **HTTP/1.1** | `--mode rh1 --transport <rdma>` | `--mode tcp1` | hyper `http1` + TLS, one request in flight per connection |

### Threads (fixed)

`--threads = vCPU count`, always. It is the executor budget (tokio worker threads in the
default arm-park modes), **not** a load axis. Both peers use the same value for a fair CPU
comparison.

### Connections

Fixed multiples of the vCPU count — the fan-out axis.

| Multiple | Example (64-vCPU VM) | What it isolates |
|---|---|---|
| **1×** | 64 | One connection per core |
| **4×** | 256 | Moderate fan-out |
| **16×** | 1024 | High fan-out — flow-control / CM-setup ceiling |

### In-flight

Requests kept outstanding **per connection** — fixed points, not a hunt.

| In-flight | What it isolates |
|---|---|
| **1** | Round-trip latency / request-response regime (no pipelining) |
| **64** | Moderate pipeline depth |
| **512** | Deep pipeline |

> **HTTP/1.1 is fixed at in-flight 1.** The `rh1`/`tcp1` path keeps exactly one request in
> flight per connection and the client always reports `in_flight = 1`, so the **{64, 512}**
> points do **not** apply to HTTP/1.1 — its concurrency scales with the connection multiple
> alone. The `{1, 64, 512}` axis applies to **echo** and **gRPC** only.

### Payload

Fixed sizes — message-rate vs bandwidth.

| Size | What it isolates |
|---|---|
| **64 B** | Per-request overhead — doorbells, completions, syscalls. Message-rate dominated. |
| **8 KiB** | Bandwidth regime — copy costs and goodput (report `Gbps`). |

> **8 KiB on the ring transports needs matched message sizing.** For `read-ring` /
> `credit-ring`, the `echo` path truncates any payload larger than `--ring-max-msg`
> (default 1500 B), so 8 KiB ring runs must set `--ring-max-msg 8192` (via
> `-e bench_ring_max_msg=8192`) on **both** peers. `send-recv` sizes its buffers from
> `--payload` and is unaffected. See the [run-procedure](run-procedure.md).

## The grid

Every transport path is run at **every** coordinate — no bespoke tuning. Per scenario:

- **echo** and **gRPC**: 3 connection multiples × 3 in-flight × 2 payloads = **18 coordinates**.
- **HTTP/1.1**: 3 connection multiples × (in-flight fixed at 1) × 2 payloads = **6 coordinates**.

…run identically for `send-recv`, `read-ring`, `credit-ring`, and the kernel baseline, so any
two cells at the same coordinate are directly comparable. Fixed run parameters `--duration` /
`--warmup` (e.g. `duration=10 warmup=3`) are recorded per table, not swept.

The headline metrics for each cell — throughput (`req/s`), tail latency (`p50`/`p95`/`p99`),
CPU cost per op, `cores busy`, `peak RSS`, and (8 KiB) `Gbps` — are defined once in the
[results-template](results-template.md#how-to-read--fill-a-cell) and
[`metrics.md`](../bench/metrics.md).

## What a good fixed-grid run looks like

To characterize a SKU you run the **whole grid** for each transport path at that SKU:

- record every coordinate's cell from the client's `--report json`;
- a coordinate a transport cannot sustain (e.g. `credit-ring` at deep pipeline, or 16×
  connections hitting MANA RDMA-CM setup flakiness) is recorded as `n/a` / `fail`, not left
  ambiguous (see [results-template](results-template.md));
- repeat the same grid on new SKUs and commits over time — the dataset grows without changing
  the framework.

See [results-template](results-template.md) for the exact table shapes and
[run-procedure](run-procedure.md) for how to execute each coordinate.
