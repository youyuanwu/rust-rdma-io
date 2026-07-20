# Benchmark strategy

What the `rdma-io-bench` benchmarks measure, and why. This is the plan; the
numbers live under each environment tree (see the [README](README.md)).

## What we compare

Three RDMA [`Transport`](../design/rdma-transport-layer.md) implementations against
a kernel baseline:

| Transport | One-line |
|---|---|
| `send-recv` | Two-sided RDMA SEND/RECV with a per-connection recv-buffer pool |
| `read-ring` | One-sided RDMA READ ring (busy-poll capable); only doorbell buffers per connection |
| `credit-ring` | Credit-based ring with explicit credit round-trips |
| **kernel baseline** | Raw TCP (`tcp`/`tcp1`) — same application stack over a kernel socket |

The kernel baseline runs the *same* application stack over a kernel socket so the
comparison isolates the transport, not the framing.

## The three scenarios (isolate different layers)

Each scenario removes or adds layers so a number maps to a specific part of the
stack:

- **[Echo](scenarios/echo.md)** — calls the `Transport` trait directly (no gRPC /
  TLS / HTTP): one `send_copy` per request, one recv completion per response.
  Throughput and latency map 1:1 to transport behaviour — the cleanest view of the
  raw RDMA data path. Baseline: raw TCP.
- **[gRPC](scenarios/grpc.md)** (`rh2`) — tonic over the full **TLS 1.3 + HTTP/2 +
  protobuf + hyper** stack on the RDMA byte stream. HTTP/2 multiplexes many RPCs
  per connection, so both connection count and in-flight streams drive load.
  Baseline: `tcp` (same tonic + TLS stack over a kernel socket).
- **[HTTP/1.1](scenarios/h1.md)** (`rh1`) — hyper `http1` + TLS with **no stream
  multiplexing**: one request in flight per connection, so load scales with
  connection count only. The structural opposite of `rh2`. Baseline: `tcp1`.

## The regime matrix

Within each scenario, results are grouped by **regime** — a specific measured
configuration. The dimensions swept:

| Dimension | Typical points | What it exposes |
|---|---|---|
| **Payload size** | 64 B · 8 KiB | message-rate (per-op overhead) vs bandwidth ceiling |
| **In-flight / pipeline depth** | 1 · 4 · 16 · 64 (echo) | how outstanding requests per connection convert to throughput |
| **Connection count** | tens → hundreds | fan-out scaling and the high-concurrency flow-control ceiling |
| **Completion topology** | arm-park · busy-poll · thread-per-core-park | latency vs CPU-cost trade-offs of completion delivery |

The `in-flight = 1`, small-payload point is the request/response regime the gRPC
`rh2` path effectively runs in; deep pipelining and connection scaling are where
RDMA's lower per-op cost shows up.

## Which [metrics](metrics.md) matter

Throughput (`req/s`) and tail latency (`p50`/`p99`) are the headline; **CPU cost
per operation** (`cpu_us_per_op`) and **`cores busy`** matter because RDMA's win is
often *efficiency* (kernel-bypass) rather than raw rate on a large core budget.
Peak RSS captures the registered-buffer/MR cost. See [metrics.md](metrics.md) for
exact definitions.

## Non-goals

- Not a machine-readable/queryable results dataset — these are curated prose
  write-ups. Raw JSON is archived by the tool under gitignored `build/…/archive/`.
- Not a how-to-run guide — see [methodology.md](methodology.md) and its references.
- Not micro-optimization tuning of a single transport in isolation; the point is
  cross-transport, cross-layer comparison against the kernel baseline.
