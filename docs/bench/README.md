# Benchmark scenarios

Measured benchmark write-ups for the `rdma-io-bench` tool, comparing the three
RDMA transports (`send-recv` / `read-ring` / `credit-ring`) against a kernel
baseline on the Azure MANA RoCEv2 VMs. Each scenario lives in its own subfolder
with a **scenario description** (`README.md`) and its **measured results** as one
or more **data-table files** grouped by regime; more data files are added
alongside as they are collected.

| Scenario | Modes | Baseline | Scenario | Data tables |
|---|---|---|---|---|
| **Echo** | `echo` / `echo-busy` / `echo-park` | raw TCP | [echo/README.md](echo/README.md) | [message-rate](echo/message-rate-64b.md) · [8 KiB](echo/large-payload-8kib.md) · [busy-poll](echo/busy-poll.md) · [echo-park](echo/thread-per-core-park.md) |
| **gRPC** | `rh2` (/ `rh3`) | `tcp` | [grpc/README.md](grpc/README.md) | [throughput](grpc/throughput-64b.md) · [payload](grpc/payload.md) · [CPU & memory](grpc/cpu-memory.md) |
| **HTTP/1.1** | `rh1` / `rh1-busy` / `rh1-park` | `tcp1` | [h1/README.md](h1/README.md) | [throughput](h1/throughput-64b.md) · [thread-per-core](h1/thread-per-core.md) · [8 KiB](h1/large-payload-8kib.md) · [methodology](h1/methodology.md) |

The three scenarios isolate different layers of the stack:

- **Echo** calls the [`Transport`](../design/rdma-transport-layer.md) trait
  directly (no gRPC / TLS / HTTP), so throughput and latency map 1:1 to transport
  behaviour — the cleanest view of the raw RDMA data path.
- **gRPC** (`rh2`) drives tonic over the full TLS 1.3 + HTTP/2 + protobuf stack
  on top of the RDMA byte stream.
- **HTTP/1.1** (`rh1`) drives hyper `http1` + TLS with no stream multiplexing —
  one request in flight per connection.

For the benchmark design, infrastructure, and how to run the tool, see
[../design/EchoBenchmark.md](../design/EchoBenchmark.md) (echo internals) and
`docs/Benchmark.md` in the `rdma-io-bench` repo. Run and interpretation guidance
lives in the `rdma-io-benchmarking` skill; record new findings as a data-table
file under the matching scenario folder (and link it from that scenario's
`README.md`).

## Adding / updating results

Organize by **regime/config, not by date** — the file layout is the *semantic*
axis (what was measured); git history is the *time* axis (superseded values).
Don't create date-named files: they fragment one sweep across many files and make
run-to-run comparison harder.

- **Re-measuring an existing regime** — update the table in place. If the old
  numbers are worth keeping, add a dated subsection (e.g. `### Re-validation
  (YYYY-MM-DD)`) or a date column/row, rather than a new file. Git holds the
  prior values.
- **A genuinely new config** (new NIC, transport, kernel/driver era, topology) —
  add a new data-table file named by the **config**, e.g. `multi-nic-64b.md` or
  `mana-driver-vX-64b.md`. A date belongs in the name only when the
  hardware/driver era is itself the subject.
- **Every data file** carries a run-context line at the top (date,
  `duration`/`warmup`/`threads`, reboot-clean NIC) so each table is self-dating
  without a dated filename.
- **Preserve section anchors** other docs deep-link to when moving content — e.g.
  `## The fd wall (fixed)` ([h1/methodology.md](h1/methodology.md)) and
  `## Large-payload (8 KiB) ceiling: read-ring vs TCP`
  ([echo/large-payload-8kib.md](echo/large-payload-8kib.md)).

Then link the new file from its scenario `README.md` (**Measured results** list)
and add it to the *Data tables* column of the table above. Raw result JSON is a
separate concern — the bench tool already archives it under the gitignored
`build/…/archive/`; these docs are the curated write-ups, not the raw log.
