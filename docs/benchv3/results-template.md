# Result showcase template (bench v3)

A reusable scaffold for presenting bench v3 numbers as they are collected **repeatedly, across
VM SKUs and over time**. Copy a block, fill the cells from the JSON the client emits, and commit
only the curated tables here — never raw run artifacts. The workload coordinates are fixed by the
[scenario matrix](scenario-matrix.md); this doc only defines the **table shapes** and how to fill
them. See the [run-procedure](run-procedure.md) for how to produce the numbers.

## How to read / fill a cell

Each run of the client with `--report json` prints one JSON object. Read these fields (exact
names from the tool; there is **no `p90`** and no `failed` field):

- **`throughput_rps`** — sustained requests/second. Higher is better.
- **`latency_us.p50` / `.p95` / `.p99`** — median and tail per-request latency, in microseconds.
- **`errors`** — **always check this.** A high throughput with non-zero `errors` is **not** a
  valid data point — investigate before recording it.
- **`cpu_us_per_op`** (optional) — CPU microseconds per request (`cpu_seconds / total_requests`).
- **`cpu_seconds`** (optional) — CPU-seconds over the measured window; derive
  **`cores busy = cpu_seconds / duration_secs`**.
- **`peak_rss_kb`** (optional) — peak resident set (`VmHWM`); report as MB.
- **`Gbps`** (derived, 8 KiB tables) — application goodput `= throughput_rps × payload_bytes × 8 / 1e9`.

These are the authoritative v3 metric definitions; the other v3 docs reference this section. For
the full rationale (why CPU is sampled in-process, what `cores busy` means) see
[`metrics.md`](../bench/metrics.md).

> **Recording an unsustainable coordinate.** Some coordinates a transport simply cannot sustain —
> e.g. `credit-ring` at a deep pipeline, or 16× connections hitting MANA RDMA-CM setup flakiness.
> Record such a cell as `n/a` or `fail (CM setup)` with a footnote (see the example in Table A).
> A non-numeric cell is an **expected** outcome, not a data bug.

---

## Table A — kernel-baseline-vs-RDMA comparison at one coordinate

The core comparable board: one coordinate, every transport path for the scenario side by side.
Fill one copy per `(scenario, payload, connections, in-flight)` coordinate you care about. State
the fixed (non-swept) axes in the caption; the transport path is the row dimension. The example
below is an **echo** board (6 paths); a **gRPC** board omits the `read-ring (busy-poll)` and
`read-ring (thread-per-core park)` rows (no gRPC busy/park variant).

> **SKU:** `________` · **vCPU:** `__` · **scenario:** `echo` · **payload:** 64 B ·
> **connections:** 1× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s ·
> **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv`    |                    |          |          |          |             |       |               |        |
| `read-ring` (arm-park) |            |          |          |          |             |       |               |        |
| `read-ring` (busy-poll)² |          |          |          |          |             |       |               |        |
| `read-ring` (thread-per-core park)² |   |          |          |          |             |       |               |        |
| `credit-ring`¹ |                    |          |          |          |             |       |               |        |
| kernel baseline |                   |          |          |          |             |       |               |        |

¹ Example of an unsustainable coordinate — if `credit-ring` cannot sustain this depth, record
`fail (over-queue)` / `n/a` in its cells rather than leaving them ambiguous.
² `read-ring` busy-poll / thread-per-core park are read-ring completion topologies (`echo-busy`/
`echo-park`, or `rh1-busy`/`rh1-park`); **echo & HTTP/1.1 only** — omit these rows on a gRPC board.

**Illustrative** (how an unsustainable coordinate is recorded — *not* real data):

| Transport path | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `credit-ring`  | `fail (CM setup)`  | `n/a`    | `n/a`    | `n/a`    | `n/a`       | `n/a` | `n/a`         | `n/a`  |

For **8 KiB** (bandwidth) coordinates, add a `Gbps` column after `throughput`:

> **SKU:** `________` · **vCPU:** `__` · **scenario:** `echo` · **payload:** 8 KiB ·
> **connections:** 1× vCPU · **in-flight:** 64 · **duration/warmup:** 10 s / 3 s ·
> **git commit:** `________` · **date:** `________`

| Transport path | Throughput (req/s) | Gbps | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| -------------- | ------------------ | ---- | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| `send-recv`    |                    |      |          |          |          |             |       |               |        |
| `read-ring` (arm-park) |            |      |          |          |          |             |       |               |        |
| `read-ring` (busy-poll)² |          |      |          |          |          |             |       |               |        |
| `read-ring` (thread-per-core park)² |   |      |          |          |          |             |       |               |        |
| `credit-ring`  |                    |      |          |          |          |             |       |               |        |
| kernel baseline |                   |      |          |          |          |             |       |               |        |

---

## Table B — the concurrency grid for one transport

Fill one copy **per (scenario, payload, transport)** to see how a single transport moves across
the fixed grid. The swept axis is the `(connections × in-flight)` coordinate (rows); everything
else is fixed in the caption. For **HTTP/1.1** the in-flight column is always 1, so it has three
rows (one per connection multiple) instead of nine. The `transport` axis includes the read-ring
completion topologies (`read-ring (busy-poll)` / `read-ring (thread-per-core park)`) for echo &
HTTP/1.1 boards.

> **SKU:** `________` · **vCPU:** `__` · **scenario:** `echo` · **payload:** 64 B ·
> **transport:** `read-ring` · **duration/warmup:** 10 s / 3 s · **git commit:** `________` ·
> **date:** `________`

| connections | in-flight | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | CPU/op (µs) | cores | peak RSS (MB) | errors |
| ----------- | --------- | ------------------ | -------- | -------- | -------- | ----------- | ----- | ------------- | ------ |
| 1× vCPU     | 1         |                    |          |          |          |             |       |               |        |
| 1× vCPU     | 64        |                    |          |          |          |             |       |               |        |
| 1× vCPU     | 512       |                    |          |          |          |             |       |               |        |
| 4× vCPU     | 1         |                    |          |          |          |             |       |               |        |
| 4× vCPU     | 64        |                    |          |          |          |             |       |               |        |
| 4× vCPU     | 512       |                    |          |          |          |             |       |               |        |
| 16× vCPU    | 1         |                    |          |          |          |             |       |               |        |
| 16× vCPU    | 64        |                    |          |          |          |             |       |               |        |
| 16× vCPU    | 512       |                    |          |          |          |             |       |               |        |

For an **8 KiB** (bandwidth) copy of Table B, add a `Gbps` column after `Throughput` (as in
Table A).

---

## Table C — SKU sweep for one coordinate

Fill one copy **per (scenario, payload, transport, connections, in-flight)** to see how a
transport scales with VM size. The swept axis is the SKU (rows); extend it downward as you add
SKUs.

> **scenario:** `echo` · **payload:** 64 B · **transport:** `read-ring` · **connections:** 1× vCPU ·
> **in-flight:** 64 · **duration/warmup:** 10 s / 3 s · **git commit:** `________`

| VM SKU            | vCPU / RAM  | Throughput (req/s) | p50 (µs) | p95 (µs) | p99 (µs) | errors | date |
| ----------------- | ----------- | ------------------ | -------- | -------- | -------- | ------ | ---- |
| `________`        | `__ / __`   |                    |          |          |          |        |      |
| `________`        | `__ / __`   |                    |          |          |          |        |      |

---

## Narrative template

For each published sweep, add a short write-up alongside the tables:

- **Setup:** SKU(s), vCPU count, git commit, date, duration/warmup, scenario, the fixed grid axes.
- **Headline:** the one-sentence takeaway (e.g. "at 1× connections / in-flight 64, `read-ring`
  matches the kernel baseline throughput at ~⅛ the CPU per op").
- **Baseline vs RDMA:** how each RDMA path compares to the kernel baseline on throughput, CPU
  efficiency, and tail latency at the same coordinate.
- **Where RDMA wins / loses:** call out the coordinates where kernel-bypass helps vs where the
  baseline is still ahead.
- **Caveats:** any non-numeric cells (`n/a` / `fail`), non-zero `errors`, and NIC/CPU saturation
  if the SKU looked like the bottleneck.

## Reproducing a published number

Every table cell traces back to one client-side JSON result. To re-run a cell, read the caption's
fixed axes + commit and re-issue the matching command per the [run-procedure](run-procedure.md),
pinned to the recorded `git commit`.

> **Filename collision warning.** The orchestration writes results to
> `/tmp/bench-<mode>-<transport>-<connections>conn-<threads>thr-<in_flight>if.json` — the name
> encodes **no payload and no date**, so a 64 B run followed by an 8 KiB run at the same
> coordinate (or a repeat) **overwrites** the previous file. Fill the cell immediately, or set a
> per-payload/per-run `bench_out_dir` (or rename the file to include payload + date + commit)
> before the next run.
