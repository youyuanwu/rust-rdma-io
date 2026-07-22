# azure-mana-rocev2 — RDMA-vs-TCP scoreboard

The canonical, comparison-first view of how the three RDMA transports (`send-recv`,
`read-ring`, `credit-ring`) compare against the kernel baseline (`tcp` for echo/gRPC,
`tcp1` for HTTP/1.1) on **this** environment. Results are organized as **one board per
workload**: each board leads with a cross-transport **peak-comparison summary** and a
compact **peak-finder** excerpt, and links to the workload's detail page for the full
per-transport sweeps and analysis. Schema, peak-selection, and the `mode`/`src`/coverage
rules: [collection protocol](../collection.md#12-per-workload-board-schema). The
environment definition is in the [environment README](README.md).

> **Per-environment numbers.** These figures were measured on Azure MANA RoCEv2
> 64-vCPU VMs and are **not comparable across hardware/NIC eras**. Other
> environments keep their own scoreboards.

**Summary columns:** peak throughput (req/s) · CPU/op (µs) · cores@peak · p99 (µs) · then the
derived **vs-baseline ratios** — CPU-eff× (baseline CPU/op ÷ RDMA) · p99× (RDMA ÷ baseline;
<1 = better tail) · tput% (transport peak ÷ **baseline peak**). `n/r` = not recorded; `⏳ pending`
/ `— N/A (no such mode)` per the coverage matrix. Because each transport is shown at its own peak
config, CPU-eff× / p99× are `n/r` where a matched baseline wasn't recorded at that config — the
matched-config ratios live in the dated blocks on each detail page.

> **Reading the summaries.** Each board's summary table is identical to the one on its detail page.
> The superscripts `¹²³⁴` on the peak-throughput values map, in row order, to the per-transport
> **Peak configs** line directly beneath each board (send-recv ¹, read-ring ², credit-ring ³, baseline
> ⁴), which names each transport's peak config and source date.

## The one-paragraph story

On this NIC, **TCP wins absolute small-message throughput by spending cores**, while
**RDMA wins CPU-efficiency and tail latency** — most dramatically in raw `echo`
(read-ring ~4× cheaper per op at ~1/6 the cores at a matched config). At **large 8 KiB payloads the
raw `echo` result inverts** and read-ring wins bandwidth outright (~36 vs ~30 Gbps). Once the full
**gRPC** TLS+HTTP/2+protobuf stack is added the per-op cost becomes transport-agnostic (RDMA's raw
win is masked) and TCP takes the 8 KiB headline; in **HTTP/1.1** (one request per connection) TCP
brute-forces the peak with thousands of connections while RDMA delivers ~75–94% of it at a quarter
the latency and far fewer cores — and pinned read-ring thread-per-core (`rh1-busy`) beats the kernel
~4× at a matched core budget (185% of TCP's own peak at just 16 cores).

---

## Echo (raw `Transport`, baseline `tcp`)

The cleanest view of the RDMA data path — no gRPC/TLS/HTTP.

### Echo message-rate 64 B → [detail](echo/message-rate-64b.md)

Per-op overhead (doorbells/completions) is the limiter. Read-ring folds three completion modes
(arm-park / busy-poll / park).

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 4.40M¹ | 1.30 µs | ~5.7 | 1408 µs | 4.1× | 1.0× | 49% |
| read-ring (arm-park) | **6.83M**² | 1.38 µs | 9.4 | n/r | n/r | n/r | 76% |
| credit-ring | 1.36M³ | n/r | n/r | n/r | n/r | n/r | 15% |
| tcp | **8.94M**⁴ | n/r | ~55 | n/r | — | — | baseline |

Peak configs: send-recv 64×64 · read-ring 48×512 (arm-park; knee ~32×512) · credit-ring 8×8 IF16 ·
tcp 512×64. Matched-config efficiency (64×64): read-ring 4.2× CPU-eff / 70% tput. read-ring
peak-finder (full sweeps → [detail](echo/message-rate-64b.md)):

| config | mode | throughput | CPU/op | cores | p50 | p99 | src |
|---|---|---:|---:|---:|---:|---:|---|
| 32×512 | arm-park | 6.75M | 1.051 µs | 7.1 | 462 µs | 1942 µs | Undated |
| **48×512** | **arm-park** | **6.83M** | 1.380 µs | 9.4 | 651 µs | n/r | Undated |
| 16c · 64×64 | busy-poll | 5.39M | 2.97 µs | 16 | 427 µs | 2403 µs | Undated |

### Echo large-payload 8 KiB → [detail](echo/large-payload-8kib.md)

Bandwidth-bound; read-ring wins on all axes.

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | ⏳ pending | — | — | — | — | — | — |
| read-ring | **549k**¹ | 5.4 µs | ~3.0 | 922 µs | n/r | n/r | 121% |
| credit-ring | ⏳ pending | — | — | — | — | — | — |
| tcp | 454k² | 8.8 µs | ~4.0 | 439 µs | — | — | baseline |

Peak configs: read-ring 24×16 (**36.0 Gbps**) · tcp 32×4 (29.8 Gbps); send-recv / credit-ring ⏳
pending at 8 KiB. read-ring peak-finder (full sweeps → [detail](echo/large-payload-8kib.md)):

| config | throughput | CPU/op | cores | p50 | p99 | Gbps | src |
|---|---:|---:|---:|---:|---:|---:|---|
| 24×8 | 539k | 5.0 µs | ~2.7 | 259 µs | 607 µs | 35.3 | Undated |
| **24×16** | **549k** | 5.4 µs | ~3.0 | 408 µs | 922 µs | **36.0** | Undated |

---

## gRPC (`rh2` — tonic over TLS 1.3 + HTTP/2 + protobuf, baseline `tcp`)

The full stack masks RDMA's per-op efficiency; both transports are stack-bound.

### gRPC throughput 64 B → [detail](grpc/throughput-64b.md)

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 510.2K¹ | 85.6 µs | n/r | n/r | 0.9× | n/r | 63% |
| read-ring | **833K**² | 67.4 µs | ~56 | 8567 µs | 1.1× | 0.6× | 103% |
| credit-ring | 710.3K³ | n/r | n/r | n/r | n/r | n/r | 88% |
| tcp | **806K**⁴ | n/r | ~55 | n/r | — | — | baseline |

Peak configs: send-recv 64×8 · read-ring 192×16 · credit-ring 64×64 · tcp 256×16. read-ring
peak-finder (full sweeps → [detail](grpc/throughput-64b.md)):

| config | throughput | CPU/op | cores | p50 | p99 | src |
|---|---:|---:|---:|---:|---:|---|
| 128×16 | 784K | 70.0 µs | n/r | n/r | 5635 µs | 2026-07-06 |
| **192×16** | **833K** | 67.4 µs | ~56 | n/r | 8567 µs | 2026-07-06 |
| 208×16 | 828K | 67.6 µs | n/r | n/r | 9431 µs | 2026-07-06 |

### gRPC payload 8 KiB → [detail](grpc/payload.md)

TCP wins the 8 KiB gRPC headline (rings hit the high-fan-out deadlock); rings win latency ~10×.

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 114K¹ | n/r | n/r | n/r | n/r | n/r | 26% |
| read-ring | 247K² | 95 µs | ~24 | 1090 µs | 1.2× | 0.1× | 57% |
| credit-ring | 204K³ | n/r | n/r | n/r | n/r | n/r | 47% |
| tcp | **431K**⁴ | n/r | n/r | n/r | — | — | baseline |

Peak configs: send-recv 192×1 (2026-07-17; Undated full-metric 110K) · read-ring 128×1 (247K, Undated;
16.2 Gbps) · credit-ring 48×1 (2026-07-17; Undated 202K) · tcp 256×8 (2026-07-17; Undated 427K,
28.3 Gbps). read-ring peak-finder (full sweeps → [detail](grpc/payload.md)):

| config | throughput | CPU/op | cores | p50 | p99 | Gbps | src |
|---|---:|---:|---:|---:|---:|---:|---|
| 96×1 | 229K | n/r | n/r | n/r | n/r | n/r | Undated |
| **128×1** | **247K** | 95 µs | ~24 | 486 µs | 1090 µs | 16.2 | Undated |

### gRPC client CPU & memory → [detail](grpc/cpu-memory.md)

A CPU/RSS characterization (in-flight 1, 64×64); CPU/op is transport-agnostic.

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 196.3K | 65.3 µs | n/r | n/r | 1.1× | n/r | 91% |
| read-ring | 236.1K | 65.9 µs | n/r | n/r | 1.1× | n/r | 109% |
| credit-ring | 248.6K | 70.8 µs | n/r | n/r | 1.1× | n/r | 115% |
| tcp | 216.3K | 74.9 µs | n/r | n/r | — | — | baseline |

Peak RSS (in-flight 64): send-recv 166 MB · read-ring 102 MB · credit-ring 102 MB · tcp 98 MB.
read-ring depth-scaling peak-finder (full sweeps → [detail](grpc/cpu-memory.md)):

| config | throughput | CPU/op | cores | p50 | p99 | peak RSS | src |
|---|---:|---:|---:|---:|---:|---:|---|
| in-flight 1 | n/r | 66.5 µs | n/r | n/r | n/r | n/r | 2026-07-17 |
| in-flight 64 | n/r | 41.5 µs | n/r | n/r | n/r | 102 MB | 2026-07-17 |
| **64×64, IF1** | **236.1K** | 65.9 µs | n/r | n/r | n/r | 51 MB | 2026-07-17 |

---

## HTTP/1.1 (`rh1` — hyper http1 + TLS, one request per connection, baseline `tcp1`)

Connection-count bound: throughput ≈ connections / RTT. Cores are the reported cost (not CPU/op),
so CPU-eff× is `n/r` for the non-pinned rows.

### HTTP/1.1 throughput 64 B → [detail](h1/throughput-64b.md)

Read-ring folds the pinned thread-per-core modes (`rh1-busy` / `rh1-park`).

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 315k¹ | n/r | n/r | n/r | n/r | n/r | 40% |
| read-ring (rh1-busy) | **1461K**² | 10.9 µs | 16 | n/r | 2.1× | n/r | 185% |
| credit-ring | 360k³ | n/r | n/r | n/r | n/r | n/r | 46% |
| tcp1 | **790k**⁴ | n/r | ~26 | n/r | — | — | baseline |

Peak configs: send-recv 128c · read-ring 16 pinned cores/128 conns (rh1-busy) · credit-ring 128c ·
tcp1 5120c. read-ring peak-finder (full sweeps → [detail](h1/throughput-64b.md)):

| config | mode | throughput | CPU/op | cores | p50 | p99 | src |
|---|---|---:|---:|---:|---:|---:|---|
| 512c | rh1 | 582K | n/r | ~12 | 856 µs | 1656 µs | Undated |
| 8c · 64 | rh1-busy | 852K | 9.4 µs | 8 | n/r | n/r | 2026-07-17 |
| **16c · 128** | **rh1-busy** | **1461K** | 10.9 µs | 16 | n/r | n/r | 2026-07-17 |

### HTTP/1.1 large-payload 8 KiB → [detail](h1/large-payload-8kib.md)

Bandwidth-bound; read-ring close behind tcp1 at far fewer connections, no deadlock.

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 322k¹ | n/r | n/r | n/r | n/r | n/r | 74% |
| read-ring | 410K² | n/r | ~14 | 1954 µs | n/r | 0.5× | 94% |
| credit-ring | 321k³ | n/r | n/r | n/r | n/r | n/r | 73% |
| tcp1 | **437k**⁴ | n/r | ~19 | 4247 µs | — | — | baseline |

Peak configs: send-recv 256c · read-ring 384c (**26.9 Gbps**) · credit-ring 128c · tcp1 1024c
(28.6 Gbps). read-ring peak-finder (full sweeps → [detail](h1/large-payload-8kib.md)):

| config | throughput | CPU/op | cores | p50 | p99 | Gbps | src |
|---|---:|---:|---:|---:|---:|---:|---|
| 320c | 388K | n/r | n/r | n/r | n/r | 25.5 | Undated |
| **384c** | **410K** | n/r | ~14 | 889 µs | 1954 µs | 26.9 | Undated |

---

## Coverage matrix

Which scenario × workload × transport cells have been collected. `✅` collected · `⏳` pending
(collectible, not yet run) · `— N/A` not applicable (no such completion mode). Completion-mode runs
(`echo-busy`/`echo-park`, `rh1-busy`/`rh1-park`) are folded into their base workload's read-ring
`mode` column, so they are marked on the base-workload rows below. The kernel baseline (`tcp`/`tcp1`)
is measured for every workload.

| scenario | workload | send-recv | read-ring | credit-ring | baseline |
|---|---|:---:|:---:|:---:|:---:|
| echo | message-rate 64 B | ✅ | ✅ | ✅ | ✅ |
| echo | ↳ busy-poll `echo-busy` ᵐ | — N/A | ✅ | — N/A | ✅ |
| echo | ↳ thread-per-core `echo-park` ᵐ | — N/A | ✅ | — N/A | ✅ |
| echo | large-payload 8 KiB | ⏳ | ✅ | ⏳ | ✅ |
| grpc | throughput 64 B | ✅ | ✅ | ✅ | ✅ |
| grpc | payload 8 KiB | ✅ | ✅ | ✅ | ✅ |
| grpc | client CPU & memory | ✅ | ✅ | ✅ | ✅ |
| h1 | throughput 64 B | ✅ | ✅ | ✅ | ✅ |
| h1 | ↳ thread-per-core `rh1-busy`/`rh1-park` ᵐ | — N/A | ✅ | — N/A | ✅ |
| h1 | large-payload 8 KiB | ✅ | ✅ | ✅ | ✅ |

ᵐ Completion-mode run — folded into its base workload's board via the read-ring `mode` column;
`send-recv`/`credit-ring` have `— N/A (no such mode)`. `echo/large-payload-8kib` is the one
`⏳ pending` gap — send-recv and credit-ring at 8 KiB are collectible but not yet run.

To add data, follow the [collection protocol](../collection.md): prepend a dated block to the
workload's detail page, then refresh its board (summary + tuning) and this coverage matrix.
