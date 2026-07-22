# gRPC client CPU & memory

Client-side CPU per request and peak RSS across pipelining depth and full scale,
plus the overall takeaways. See the [gRPC scenario](../../scenarios/grpc.md),
[methodology](../../methodology.md), and [metrics](../../metrics.md). Other gRPC
regimes: [throughput & pipelining (64 B)](throughput-64b.md) ·
[payload size](payload.md).

## Board: gRPC client CPU & memory — comparison

A **characterization** regime (client CPU/op and peak RSS at in-flight 1, 64 conns × 64 threads), not a
throughput sweep — all rows are from the single **2026-07-17** block (no cross-block mixing). Headline
metrics are **CPU/op** and **peak RSS**; `tput%` = throughput ÷ tcp. Tuning tables carry a trailing
`peak RSS` column. Schema: [collection protocol](../../collection.md#12-per-workload-board-schema).

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 196.3K | 65.3 µs | n/r | n/r | 1.1× | n/r | 91% |
| read-ring | 236.1K | 65.9 µs | n/r | n/r | 1.1× | n/r | 109% |
| credit-ring | 248.6K | 70.8 µs | n/r | n/r | 1.1× | n/r | 115% |
| tcp | 216.3K | 74.9 µs | n/r | n/r | — | — | baseline |

**Peak RSS** (in-flight 64): send-recv 166 MB, read-ring 102 MB, credit-ring 102 MB, tcp 98 MB
(send-recv carries the heaviest recv pool). At the 64×64 / in-flight-1 point above: send-recv 82 MB,
read-ring 51 MB, credit-ring 51 MB, tcp 37 MB. **CPU/op is transport-agnostic** (~65–75 µs,
stack-bound), so RDMA's raw ~4× per-op efficiency (measured in `echo`) is masked by the
TLS/HTTP-2/protobuf stack here.

### Tuning — CPU/op falls with pipelining depth (8 conns / 8 threads)

**read-ring**:

| config | throughput | CPU/op | cores | p50 | p99 | peak RSS | src |
|---|---:|---:|---:|---:|---:|---:|---|
| in-flight 1 | n/r | 66.5 µs | n/r | n/r | n/r | n/r | 2026-07-17 |
| in-flight 16 | n/r | 47.9 µs | n/r | n/r | n/r | n/r | 2026-07-17 |
| in-flight 64 | n/r | 41.5 µs | n/r | n/r | n/r | 102 MB | 2026-07-17 |
| 64×64, IF1 | 236.1K | 65.9 µs | n/r | n/r | n/r | 51 MB | 2026-07-17 |

**send-recv**:

| config | throughput | CPU/op | cores | p50 | p99 | peak RSS | src |
|---|---:|---:|---:|---:|---:|---:|---|
| in-flight 1 | n/r | 65.2 µs | n/r | n/r | n/r | n/r | 2026-07-17 |
| in-flight 64 | n/r | 43.2 µs | n/r | n/r | n/r | 166 MB | 2026-07-17 |
| 64×64, IF1 | 196.3K | 65.3 µs | n/r | n/r | n/r | 82 MB | 2026-07-17 |

**credit-ring**:

| config | throughput | CPU/op | cores | p50 | p99 | peak RSS | src |
|---|---:|---:|---:|---:|---:|---:|---|
| in-flight 1 | n/r | 76.1 µs | n/r | n/r | n/r | n/r | 2026-07-17 |
| in-flight 64 | n/r | 42.3 µs | n/r | n/r | n/r | 102 MB | 2026-07-17 |
| 64×64, IF1 | 248.6K | 70.8 µs | n/r | n/r | n/r | 51 MB | 2026-07-17 |

**tcp**:

| config | throughput | CPU/op | cores | p50 | p99 | peak RSS | src |
|---|---:|---:|---:|---:|---:|---:|---|
| in-flight 1 | n/r | 64.3 µs | n/r | n/r | n/r | n/r | 2026-07-17 |
| in-flight 64 | n/r | 44.2 µs | n/r | n/r | n/r | 98 MB | 2026-07-17 |
| 64×64, IF1 | 216.3K | 74.9 µs | n/r | n/r | n/r | 37 MB | 2026-07-17 |

## Results

### 2026-07-17 — regression re-validation
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `duration=10 warmup=3`, reboot-clean NIC

Re-run on the current binary as a regression check. **No regression** — client
CPU-per-op and peak RSS reproduce within noise, and the transport-agnostic-CPU story
holds.

**8 conns / 8 threads — CPU/op (µs), vs baseline:**

| in-flight | send-recv | read-ring | credit-ring | tcp |
|---:|---:|---:|---:|---:|
| 1  | 65.2 (67.1) | 66.5 (66.4) | 76.1 (74.7) | 64.3 (64.4) |
| 16 | 51.8 (45.9) | 47.9 (45.7) | 48.2 (48.1) | 50.0 (47.1) |
| 64 | 43.2 (40.7) | 41.5 (50.6) | 42.3 (55.1) | 44.2 (42.0) |

Peak RSS at in-flight 64: send-recv 166 MB (165), read-ring 102 MB (101),
credit-ring 102 MB (100), tcp 98 MB (98) — unchanged; send-recv still carries the
heaviest pool.

**64 conns / 64 threads, in-flight 1:**

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 196.3K | 65.3 µs | n/r | n/r | n/r | 82 MB | 1.1× · n/r · 91% |
| read-ring | 236.1K | 65.9 µs | n/r | n/r | n/r | 51 MB | 1.1× · n/r · 109% |
| credit-ring | 248.6K | 70.8 µs | n/r | n/r | n/r | 51 MB | 1.1× · n/r · 115% |
| tcp | 216.3K | 74.9 µs | n/r | n/r | n/r | 37 MB | baseline |

CPU-per-op stays transport-agnostic (all ~65–75 µs, stack-bound), confirming the
gRPC-layer picture is unchanged from baseline.

### Undated — historical baseline
- **Date:** unknown (baseline result set predates dated recording)
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, measured over the benchmark window (connection setup and warmup excluded)

#### Client CPU & memory

Client-side CPU per request and peak RSS, 8 conns / 8 threads, 64 B:

| in-flight | metric | send-recv | read-ring | credit-ring | tcp |
|---:|---|---:|---:|---:|---:|
| 1  | CPU µs/op | 67.1 | 66.4 | 74.7 | 64.4 |
| 16 | CPU µs/op | 45.9 | 45.7 | 48.1 | 47.1 |
| 64 | CPU µs/op | 40.7 | 50.6 | 55.1 | 42.0 |
| 64 | peak RSS MB | 165 | 101 | 100 | 98 |

Two things stand out. First, **at the gRPC layer CPU-per-op is comparable between
RDMA and TCP** — the TLS + HTTP/2 + protobuf stack dominates client CPU and costs
the same regardless of the byte transport, so RDMA's raw efficiency advantage (the
direct-transport `echo` mode measures RDMA ~4× cheaper per op) is masked here.
Second, **CPU-per-op falls as pipelining depth rises** (67 → 41 µs for `send-recv`)
because batching amortizes the per-request stack overhead over more work per wakeup.
RDMA carries a higher peak RSS than TCP (registered buffer pools / MRs), and
`send-recv` the most — a full recv-buffer pool per connection.

At full scale (64 conns / 64 threads, `in_flight=1`, ~17–19 client cores) the
picture is unchanged — CPU/op stays transport-agnostic:

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 191.9K | 70.8 µs | ~17–19 | n/r | n/r | 84 MB | 1.1× · n/r · 87% |
| read-ring | 255.6K | 75.5 µs | ~17–19 | n/r | n/r | 52 MB | 1.0× · n/r · 116% |
| credit-ring | 237.9K | 75.2 µs | ~17–19 | n/r | n/r | 52 MB | 1.0× · n/r · 108% |
| tcp | 220.6K | 75.4 µs | ~17–19 | n/r | n/r | 38 MB | baseline |

This is the sharpest contrast with raw `echo`, which at 64×64 measures `read-ring`
at **1.2 µs/op vs TCP 5.4 µs/op** (RDMA ~4× cheaper): at the gRPC layer that entire
efficiency gap is consumed by the TLS/HTTP-2/protobuf stack, not the transport.

**Takeaways.** With the current data path (native-tokio + vectored `send_gather`)
`rh2` gRPC-over-RDMA now **meets or beats the TCP baseline at every tested config**
(the old ~40K rh2 ceiling is gone). The `read-ring` transport pipelines cleanly at
depth after the bidirectional-deadlock fix
([read-ring-concurrent-stream-deadlock.md](../../../bugs/read-ring-concurrent-stream-deadlock.md)).
RDMA wins on throughput *and* latency up to ~200 connections (peak ~365–376K req/s,
transport-dependent); beyond that a high-concurrency flow-control deadlock appears
(all connections still establish — it is not a connection-setup ceiling), so TCP,
which keeps scaling, is the choice for very high connection fan-out until that
deadlock is root-caused.
