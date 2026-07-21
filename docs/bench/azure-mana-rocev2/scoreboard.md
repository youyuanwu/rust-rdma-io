# azure-mana-rocev2 — RDMA-vs-TCP scoreboard

The canonical, comparison-first view of how the three RDMA transports (`send-recv`,
`read-ring`, `credit-ring`) compare against the kernel baseline (`tcp` for echo/gRPC,
`tcp1` for HTTP/1.1) on **this** environment. Every row is one regime × transport in
the canonical **scoreboard projection** (see the
[collection protocol](../collection.md) for the schema, the value-selection policy,
and the regime kinds). Full data, sweeps, and analysis live on each linked regime
detail page; the environment definition is in the [environment README](README.md).

> **Per-environment numbers.** These figures were measured on Azure MANA RoCEv2
> 64-vCPU VMs and are **not comparable across hardware/NIC eras**. Other
> environments keep their own scoreboards.

**Columns:** throughput (req/s) · CPU/op (µs) · cores@peak · p99 (µs) · then the
derived **vs-baseline ratios** — CPU-eff× (baseline CPU/op ÷ RDMA) · p99×
(RDMA p99 ÷ baseline; <1 = better tail) · tput% (RDMA ÷ baseline). `n/r` = not
recorded in the source block; `⏳ pending` / `— N/A (no such mode)` per the coverage
matrix below.

## The one-paragraph story

On this NIC, **TCP wins absolute small-message throughput by spending cores**, while
**RDMA wins CPU-efficiency and tail latency** — most dramatically in raw `echo`
(read-ring ~4× cheaper per op at ~1/6 the cores). At **large 8 KiB payloads the raw
`echo` result inverts** and read-ring wins bandwidth outright (~36 vs ~30 Gbps). Once
the full **gRPC** TLS+HTTP/2+protobuf stack is added the per-op cost becomes
transport-agnostic (RDMA's raw win is masked) and TCP takes the 8 KiB headline; in
**HTTP/1.1** (one request per connection) TCP brute-forces the peak with thousands of
connections while RDMA delivers ~75% of it at a quarter the latency and far fewer
cores — and pinned read-ring thread-per-core (`rh1-busy`) beats the kernel ~4× at a
matched core budget.

## Echo (raw `Transport`, baseline `tcp`)

The cleanest view of the RDMA data path — no gRPC/TLS/HTTP.

| regime | transport | throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| [message-rate 64 B](echo/message-rate-64b.md)¹ | send-recv | 4.14M | 1.25 µs | 5.2 | 1502 µs | 4.2× | 1.1× | 61% |
| [message-rate 64 B](echo/message-rate-64b.md) | read-ring | 4.77M | 1.24 µs | 5.9 | 1760 µs | 4.2× | 1.3× | 70% |
| [message-rate 64 B](echo/message-rate-64b.md) | credit-ring | 0.98M | 3.18 µs | 3.1 | 4731 µs | 1.6× | 3.5× | 14% |
| [message-rate 64 B](echo/message-rate-64b.md) | **tcp** | 6.84M | 5.20 µs | 35.5 | 1339 µs | — | — | baseline |
| [large-payload 8 KiB](echo/large-payload-8kib.md)¹ | send-recv | ⏳ pending | — | — | — | — | — | — |
| [large-payload 8 KiB](echo/large-payload-8kib.md) | read-ring | 533k | 4.95 µs | ~2.6 | 619 µs | 1.8× | 1.4× | 118% |
| [large-payload 8 KiB](echo/large-payload-8kib.md) | credit-ring | ⏳ pending | — | — | — | — | — | — |
| [large-payload 8 KiB](echo/large-payload-8kib.md) | **tcp** | 452k | 8.91 µs | ~4.0 | 443 µs | — | — | baseline |
| [busy-poll `echo-busy`](echo/busy-poll.md)¹ ᵗ | send-recv | — N/A (no such mode) | — | — | — | — | — | — |
| [busy-poll `echo-busy`](echo/busy-poll.md) | read-ring | 5.39M | 2.97 µs | 16 | 2381 µs | n/r | n/r | 117% |
| [busy-poll `echo-busy`](echo/busy-poll.md) | credit-ring | — N/A (no such mode) | — | — | — | — | — | — |
| [busy-poll `echo-busy`](echo/busy-poll.md) | **tcp** | 4.59M | n/r | 16 | n/r | — | — | baseline |
| [thread-per-core `echo-park`](echo/thread-per-core-park.md)¹ ᵗ | send-recv | — N/A (no such mode) | — | — | — | — | — | — |
| [thread-per-core `echo-park`](echo/thread-per-core-park.md) | read-ring | 4.96M | 1.52 µs | 8 | 1655 µs | 1.8× | n/r | 192% |
| [thread-per-core `echo-park`](echo/thread-per-core-park.md) | credit-ring | — N/A (no such mode) | — | — | — | — | — | — |
| [thread-per-core `echo-park`](echo/thread-per-core-park.md) | **tcp** | 2.59M | 2.81 µs | 8 | n/r | — | — | baseline |

¹ Source block: 2026-07-17. ᵗ Completion-topology regime — a read-ring
completion-mode study (`echo-busy` / `echo-park`); the other RDMA transports have no
such mode. **message-rate** values are at the **64×64, in-flight-64 matched config**
(where CPU/op, p50/p99 are jointly recorded); each transport's *peak message rate* is
higher — read-ring ~6.75M (32×512), tcp ~8.9M (512×64), credit-ring ~1.36M, send-recv
~3.98M — with the same ~70% read-ring/TCP ratio (see the detail page). Other
best-config notes (read-ring's 8 KiB 24×8 peak, busy-poll's efficient 8-core ceiling)
are on the detail pages.

## gRPC (`rh2` — tonic over TLS 1.3 + HTTP/2 + protobuf, baseline `tcp`)

The full stack masks RDMA's per-op efficiency; both transports are stack-bound.

| regime | transport | throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| [throughput 64 B](grpc/throughput-64b.md)² | send-recv | 510.2K | 85.6 µs | n/r | n/r | 0.9× | n/r | 69% |
| [throughput 64 B](grpc/throughput-64b.md) | read-ring | 833K | 67.4 µs | ~56 | 8567 µs | 1.1× | 0.6× | 113% |
| [throughput 64 B](grpc/throughput-64b.md) | credit-ring | 645.8K | 75.9 µs | n/r | n/r | 1.0× | n/r | 87% |
| [throughput 64 B](grpc/throughput-64b.md) | **tcp** | 740K | 74.1 µs | ~54.8 | 13527 µs | — | — | baseline |
| [payload 8 KiB](grpc/payload.md)³ | send-recv | 110K | 117 µs | ~13 | 2623 µs | 1.0× | 0.3× | 26% |
| [payload 8 KiB](grpc/payload.md) | read-ring | 247K | 95 µs | ~24 | 1090 µs | 1.2× | 0.1× | 58% |
| [payload 8 KiB](grpc/payload.md) | credit-ring | 202K | 94 µs | ~19 | 422 µs | 1.3× | 0.04× | 47% |
| [payload 8 KiB](grpc/payload.md) | **tcp** | 427K | 118 µs | ~50 | 10167 µs | — | — | baseline |
| [client CPU & memory 64 B](grpc/cpu-memory.md)¹ | send-recv | 196.3K | 65.3 µs | n/r | n/r | 1.1× | n/r | 91% |
| [client CPU & memory 64 B](grpc/cpu-memory.md) | read-ring | 236.1K | 65.9 µs | n/r | n/r | 1.1× | n/r | 109% |
| [client CPU & memory 64 B](grpc/cpu-memory.md) | credit-ring | 248.6K | 70.8 µs | n/r | n/r | 1.1× | n/r | 115% |
| [client CPU & memory 64 B](grpc/cpu-memory.md) | **tcp** | 216.3K | 74.9 µs | n/r | n/r | — | — | baseline |

² Source block: 2026-07-06 (full-metric peak). ³ Source block: Undated baseline
(full-metric ceiling; the 2026-07-17 re-validation matched it). At 8 KiB TCP wins the
gRPC headline (RDMA rings hit the high-fan-out flow-control deadlock); the rings win
latency ~10× at their peak.

## HTTP/1.1 (`rh1` — hyper http1 + TLS, one request per connection, baseline `tcp1`)

Connection-count bound: throughput ≈ connections / RTT. CPU/op is not the reported
metric here (cores are), so CPU-eff× is `n/r` for the scaling regimes.

| regime | transport | throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| [throughput 64 B](h1/throughput-64b.md)³ | send-recv | 310K | n/r | ~5.5 | 702 µs | n/r | 0.1× | 40% |
| [throughput 64 B](h1/throughput-64b.md) | read-ring | 582K | n/r | ~12 | 1656 µs | n/r | 0.2× | 75% |
| [throughput 64 B](h1/throughput-64b.md) | credit-ring | 353K | n/r | ~7.3 | 766 µs | n/r | 0.1× | 45% |
| [throughput 64 B](h1/throughput-64b.md) | **tcp1** | 780K | n/r | ~26 | 7371 µs | — | — | baseline |
| [large-payload 8 KiB](h1/large-payload-8kib.md)³ | send-recv | 308K | n/r | ~8 | 1380 µs | n/r | 0.3× | 70% |
| [large-payload 8 KiB](h1/large-payload-8kib.md) | read-ring | 410K | n/r | ~14 | 1954 µs | n/r | 0.5× | 94% |
| [large-payload 8 KiB](h1/large-payload-8kib.md) | credit-ring | 314K | n/r | ~10 | 874 µs | n/r | 0.2× | 72% |
| [large-payload 8 KiB](h1/large-payload-8kib.md) | **tcp1** | 437K | n/r | ~19 | 4247 µs | — | — | baseline |
| [thread-per-core `rh1-busy`](h1/thread-per-core.md)¹ ᵗ | send-recv | — N/A (no such mode) | — | — | — | — | — | — |
| [thread-per-core `rh1-busy`](h1/thread-per-core.md) | read-ring | 1461K | 10.9 µs | 16 | n/r | 2.1× | n/r | 423% |
| [thread-per-core `rh1-busy`](h1/thread-per-core.md) | credit-ring | — N/A (no such mode) | — | — | — | — | — | — |
| [thread-per-core `rh1-busy`](h1/thread-per-core.md) | **tcp1** | 345K | 22.8 µs | 16 | n/r | — | — | baseline |

¹ Source block: 2026-07-17. ³ Source block: Undated baseline (full-metric ceiling).
ᵗ Completion-topology regime (read-ring `rh1-busy`/`rh1-park` pinned modes; `tcp1` has
no thread-per-core mode).

## Coverage matrix

Which scenario × regime × transport cells have been collected. `✅` collected · `⏳`
pending (collectible, not yet run) · `— N/A` not applicable (no such mode). The kernel
baseline (`tcp`/`tcp1`) is measured for every regime.

| scenario | regime | send-recv | read-ring | credit-ring | baseline |
|---|---|:---:|:---:|:---:|:---:|
| echo | message-rate 64 B | ✅ | ✅ | ✅ | ✅ |
| echo | large-payload 8 KiB | ⏳ | ✅ | ⏳ | ✅ |
| echo | busy-poll ᵗ | — N/A | ✅ | — N/A | ✅ |
| echo | thread-per-core ᵗ | — N/A | ✅ | — N/A | ✅ |
| grpc | throughput 64 B | ✅ | ✅ | ✅ | ✅ |
| grpc | payload 8 KiB | ✅ | ✅ | ✅ | ✅ |
| grpc | client CPU & memory | ✅ | ✅ | ✅ | ✅ |
| h1 | throughput 64 B | ✅ | ✅ | ✅ | ✅ |
| h1 | large-payload 8 KiB | ✅ | ✅ | ✅ | ✅ |
| h1 | thread-per-core ᵗ | — N/A | ✅ | — N/A | ✅ |

ᵗ Completion-topology regimes: `send-recv`/`credit-ring` are `— N/A` because they have
no busy-poll / thread-per-core completion mode. `echo/large-payload-8kib` is the one
`⏳ pending` gap — send-recv and credit-ring at 8 KiB are collectible but not yet run.

To add data, follow the [collection protocol](../collection.md): prepend a dated block
to the regime detail page, then flip the affected coverage cell here.
