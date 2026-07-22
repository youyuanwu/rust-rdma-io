# HTTP/1.1 thread-per-core (`rh1-busy` / `rh1-park`)

Pinned per-core topologies for HTTP/1.1: shared-CQ busy-poll (`rh1-busy`) and
reactor-per-core arm-park (`rh1-park`), with matched-core, ceiling, and idle-load
comparisons. See the [HTTP/1.1 scenario](../../scenarios/h1.md),
[methodology](../../methodology.md), and [metrics](../../metrics.md). Other HTTP/1.1
regimes: [throughput (64 B)](throughput-64b.md) ·
[large-payload (8 KiB)](large-payload-8kib.md).

> **Folded run.** `rh1-busy` / `rh1-park` are read-ring completion modes of the
> **[HTTP/1.1 throughput 64 B board](throughput-64b.md)** — their sweeps appear in that board's
> read-ring `mode` column. This page is the data home for the full thread-per-core sweeps, the
> matched-core `tcp1` comparison, and mode-selection guidance.

## Results

### 2026-07-17 — regression re-validation
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `duration=10 warmup=3`, reboot-clean NIC

Re-run on the current binary as a regression check. **No regression** — the pinned
modes reproduce baseline; the only gaps are the 32-core / 256-conn points, which
failed to *establish* (CM setup) on this flaky host.

**Headline (canonical schema)** — read-ring thread-per-core busy-poll (`rh1-busy`) vs kernel `tcp1` at the busy-poll ceiling (16 cores / 128 conns):

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | — N/A (no such mode) | — | — | — | — | — | — |
| read-ring (rh1-busy) | 1461K | 10.9 µs | 16 | n/r | n/r | n/r | 2.1× · n/r · 423% |
| credit-ring | — N/A (no such mode) | — | — | — | — | — | — |
| tcp1 | 345K | 22.8 µs | 16 | n/r | n/r | n/r | baseline |

Best config: `rh1-busy` peaks ~1.46M at 16 pinned cores; `rh1-park` ceiling ~579K at
8 cores; matched-core (4c/32) `rh1-busy` 431K vs `tcp1` 169K. `tcp1` has no
thread-per-core mode (kernel baseline at the same core budget).

**Matched-core (4 cores, 32 conns):**

| mode | throughput | p50 | p99 | µs/op |
|---|---:|---:|---:|---:|
| `tcp1` (kernel)¹ | 169K | 187 µs | 298 µs | 20.6 |
| `rh1` (shared) | 216K (194K) | 144 µs | 244 µs | 15.8 |
| `rh1-park` | 286K (303K) | 90 µs | 197 µs | 13.2 |
| **`rh1-busy`** | **431K** (437K) | **73 µs** | **100 µs** | **9.3** |

¹ `tcp1` = kernel HTTP/1.1 baseline at the same core budget (threads=4) and
connections; it has no thread-per-core mode. `rh1-busy` delivers ~2.5× its
throughput at ~half the p50 and CPU/op.

Busy-poll still wins matched-core outright (~2× the shared runtime, ~2.5× kernel
`tcp1`, ~9.3 µs/op).

**Peak / ceiling (8 conns/core):**

| cores | conns | `rh1-busy` | `rh1-park` | `tcp1` (kernel) |
|---:|---:|---|---|---|
| 4  | 32  | 431K · 9.3 µs/op | 286K · 13.2 µs/op | 169K · 20.6 µs/op |
| 8  | 64  | 852K · 9.4 µs/op | 579K · 12.8 µs/op | 250K · 22.6 µs/op |
| 16 | 128 | **1461K · 10.9 µs/op** | 385K · 16.1 µs/op ↓ | 345K · 22.8 µs/op |
| 32 | 256 | ✗ CM setup | ✗ CM setup | 407K · 26.4 µs/op |

`rh1-busy` **peaks at ~1.46M req/s at 16 pinned cores** (baseline 1.37M), scaling
near-linear at ~108K/core — ~4× kernel `tcp1` at the same 16 cores (345K) and at
~half the CPU/op; `rh1-park` holds its ~579K / 8-core ceiling then regresses at 16
cores — both unchanged from baseline. The 32-core / 256-conn ring points would not
*establish* this session (CM-setup flakiness at high fan-out), so the past-16-core
regression could not be re-measured — a setup ceiling, not a data-path result;
kernel `tcp1` scaled clean to 407K there (no CM setup to fail).

**Idle / low-load (2 cores, 2 conns):** `rh1` 24.3K (p50 80 µs), `rh1-park` 24.5K
(p50 81 µs), `rh1-busy` 48.0K (p50 41 µs), `tcp1` 22.7K (p50 85 µs) — matches
baseline (22.9 / 24.4 / 48.0K); busy-poll still doubles throughput and halves latency
over both `rh1` and kernel `tcp1` at the cost of two pinned cores.

### Undated — historical baseline
- **Date:** unknown (baseline result set predates dated recording)
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, read-ring, reboot-clean NIC, 0 errors

The `rh1` numbers run the whole hyper HTTP/1.1 + OpenSSL stack on a shared
multi-thread tokio runtime: a connection's request loop, its TLS session, and its
RDMA completion handling can all migrate across cores (tokio work-stealing).
`rh1-busy` and `rh1-park` instead **pin each connection to a core for life** — the
read-ring transport, the `AsyncRdmaStream`, the OpenSSL session, and the hyper
`http1` driver all run on one core's `current_thread` runtime — mirroring the
[`echo-busy` / `echo-park`](../../scenarios/echo.md) topologies:

- **`rh1-busy`** drives completions with a shared-CQ busy-poll `CoreDriver` per core
  (100 % core even when idle, zero hot-path syscalls, lowest latency).
- **`rh1-park`** gives each connection its own interrupt-armed CQ and the core
  **parks** in `epoll_wait` when idle (0 % idle CPU), servicing its connections'
  readiness on-core with no cross-core work-stealing (reactor-per-core).

`--threads` is the pinned-core count; connections are sharded round-robin across
them. HTTP/1.1 keeps one request in flight per connection, so offered load scales
with `--connections`.

**Headline (canonical schema)** — read-ring `rh1-busy` ceiling characterization (matched-core kernel `tcp1` comparison is in the 2026-07-17 block above):

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | — N/A (no such mode) | — | — | — | — | — | — |
| read-ring (rh1-busy) | 1373K | 11.7 µs | 16 | n/r | n/r | n/r | n/r · n/r · n/r |
| credit-ring | — N/A (no such mode) | — | — | — | — | — | — |
| tcp1 | n/r | n/r | n/r | n/r | n/r | n/r | baseline |

Best config: `rh1-busy` ceiling ~1.37M at 16 pinned cores (~108K/core, near-linear
to one 16-core socket); `rh1-park` ~576K at 8 cores. Kernel `tcp1` not measured in
this block; full mode sweeps and NUMA/SMT analysis follow.

#### Matched-core comparison (64 B, read-ring)

All three modes at the **same 4 cores**, 32 connections (8/core); reboot-clean NIC,
0 errors:

| mode | throughput | p50 | p99 | µs/op | ~cores |
|---|---:|---:|---:|---:|---:|
| `rh1` (shared runtime) | 194K | 161 µs | 261 µs | 17.5 | 3.40 |
| `rh1-park` | 303K | 88 µs | 244 µs | 12.8 | 3.87 |
| **`rh1-busy`** | **437K** | **71 µs** | **98 µs** | **9.2** | 4.00 |

At matched cores **busy-poll wins outright**: ~2.25× the throughput of the shared
runtime, ~2.3× lower p50, ~2.7× lower p99, and ~1.9× better CPU efficiency (9.2 vs
17.5 µs/op). `rh1-park` lands squarely between the two — pinned locality (no
work-stealing, warm per-core TLS/hyper state) buys ~1.6× over the shared runtime, but
paying a wakeup per completion keeps it behind busy-poll.

**Why busy-poll wins here but not for deep-pipeline `echo`.** HTTP/1.1 has no request
multiplexing, so it is *permanently* in the shallow one-in-flight regime — exactly
where [`echo`](../echo/busy-poll.md) found busy-poll ahead (2.4–2.9×). There is no
deep pipeline for the shared runtime's work-stealing to amortize the per-message
interrupt against (the crossover that let plain `echo` overtake busy-poll at
`in_flight=64` never happens for h1). Pinning also keeps each connection's OpenSSL and
hyper state on one core's cache instead of chasing it across the work-stealing pool.
Net: for a strict request/response protocol, thread-per-core — and busy-poll in
particular — is the better topology at a fixed core budget.

#### Peak throughput / ceiling (64 B, read-ring)

Scaling each mode up in pinned cores (8 conns/core; reboot-clean NIC between points;
the VMs are **64 logical CPUs** = 2 sockets × 16 physical × 2 SMT):

| cores | conns | `rh1-busy` | `rh1-park` |
|---:|---:|---|---|
| 4  | 32  | 437K · ~4.0c · 9.2 µs/op | 303K · ~3.9c · 12.8 µs/op |
| 8  | 64  | 864K · ~8.0c · 9.3 µs/op | **576K · ~7.4c · 12.9 µs/op** |
| 16 | 128 | **1373K · ~16.0c · 11.7 µs/op** | 387K · ~6.3c · 16.3 µs/op ↓ |
| 32 | 256 | 1304K · ~31.8c · 24.4 µs/op ↓ | 405K · ~6.8c · 16.9 µs/op |

All points 0 errors (the one exception, `rh1-busy` 16c at ~1.40M, carried 15/14M =
0.0001 % request errors). "↓" = regressed vs the row above.

- **`rh1-busy` ceiling ≈ 1.37M req/s at 16 pinned cores.** It scales **near-linear at
  ~108K req/s per core** (4→8→16 cores) because HTTP/1.1 is CPU-bound on the TLS +
  hyper stack (~9–12 µs/op) and busy-poll spins each completion locally with no
  wakeup. That is **~2.4–2.9× plain `rh1`** — vs the shared runtime's ~582K historical
  peak (~512 conns / ~12 cores) it is ~2.4×, and vs the reproduced 469K at 256 conns
  (re-measured this session on the same NIC/build, matching the historical table
  exactly) it is ~2.9× — reached here at just 16 dedicated cores. (Plain `rh1`'s
  384/512-conn peak points would not re-establish this session: MANA RDMA-CM setup
  goes flaky at that fan-out, so the shared runtime's headline peak sits right at the
  CM-flaky edge, whereas busy-poll hits 1.37M with only 128 connections.) Past 16
  cores `rh1-busy` **regresses** (32 cores → 1.30M) as the pool crosses the second
  NUMA socket and starts sharing SMT siblings: the per-op cost doubles (24.4 vs
  11.7 µs/op) even though 32 cores are pinned. 16 physical cores on one socket is the
  sweet spot.
- **`rh1-park` ceiling ≈ 576K req/s at 8 pinned cores** (~72K req/s per core) — about
  level with plain `rh1`'s absolute peak but at far fewer cores. Unlike busy-poll it
  **does not scale past 8 cores**: at 16 cores it *regresses* to ~390–405K and can
  only keep ~6.3 of 16 cores busy, with p50 ballooning to 300–565 µs (adding
  connections raises latency, not throughput). The arm-park wakeup path (interrupt +
  `epoll`) does not parallelise across the NUMA boundary the way busy-poll's local
  spin does, so park's useful range is the low-core, bursty regime.

#### Idle / low load (64 B, read-ring)

The cost of busy-poll is the idle core. 2 connections on 2 pinned cores:

| mode | throughput | p50 | p99 | ~cores |
|---|---:|---:|---:|---:|
| `rh1` (shared runtime) | 22.9K | 83 µs | 115 µs | 0.36 |
| `rh1-park` | 24.4K | 81 µs | 105 µs | 0.45 |
| `rh1-busy` | 48.0K | 40 µs | 51 µs | **2.00** |

Busy-poll still halves latency (p50 40 vs ~82 µs) and doubles throughput at 2
connections, but it **pins both cores at 100 %** to do it. `rh1-park` parks when idle
(~0.45 cores) and tracks the shared runtime's latency, so it is the efficient choice
for bursty / low-utilisation load; `rh1-busy` is for latency-critical or
steadily-loaded deployments that can dedicate the cores.

#### Picking a mode

- **`rh1-busy`** — lowest latency and highest throughput per core when the cores stay
  busy, and the only mode that scales cleanly to a full socket (~1.37M at 16 cores);
  costs 100 % CPU per pinned core even when idle, and don't cross the NUMA/SMT
  boundary (>16 cores here regresses). Best for latency-critical, steady-load
  HTTP/1.1.
- **`rh1-park`** — pinned locality without the idle spin (parks at ~0 % when quiet);
  ~1.6× the shared runtime at load up to its ~576K / 8-core ceiling, then stops
  scaling. Best for bursty / low-to-moderate-core load.
- **`rh1`** — the general-purpose shared multi-thread runtime; scales to thousands of
  connections across all cores (see the peak-throughput table) where the fixed
  per-core pools are not the point.

Server `--bind` must be a concrete RDMA IP for `rh1-busy` (it probes the device
context from it); `rh1-park` and `rh1` accept any bindable address.
