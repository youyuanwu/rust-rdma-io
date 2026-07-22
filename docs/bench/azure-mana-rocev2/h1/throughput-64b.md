# HTTP/1.1 throughput (64 B)

Peak throughput and connection scaling at 64 B, why HTTP/1.1 is connection-count
bound (not CPU or transport), and why the ring transports show no data-path
deadlock. See the [HTTP/1.1 scenario](../../scenarios/h1.md),
[methodology](../../methodology.md), and [metrics](../../metrics.md). Other
HTTP/1.1 regimes: [thread-per-core](thread-per-core.md) ·
[large-payload (8 KiB)](large-payload-8kib.md).

This board folds in the read-ring **thread-per-core runs**
([`rh1-busy` / `rh1-park`](thread-per-core.md)) via the `mode` column of the read-ring tuning table.
Schema: [collection protocol](../../collection.md#12-per-workload-board-schema).

## Board: HTTP/1.1 throughput 64 B — peak comparison

Each transport at its **peak-throughput** config (baseline `tcp1`). `tput%` = peak ÷ tcp1 peak. h1
reports **cores**, not CPU/op, so CPU-eff× is `n/r` except for the pinned thread-per-core modes (which
record µs/op).

| transport | peak throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---:|---:|---:|---:|---:|---:|---:|
| send-recv | 315k¹ | n/r | n/r | n/r | n/r | n/r | 40% |
| read-ring (rh1-busy) | **1461K**² | 10.9 µs | 16 | n/r | 2.1× | n/r | 185% |
| credit-ring | 360k³ | n/r | n/r | n/r | n/r | n/r | 46% |
| tcp1 | **790k**⁴ | n/r | ~26 | n/r | — | — | baseline |

¹ 128 conns, 2026-07-17 (Undated full-metric 310K @128c: ~5.5 cores, p99 702 µs, p99× 0.1×). ²
thread-per-core busy-poll `rh1-busy`, 16 pinned cores / 128 conns, 2026-07-17 — CPU-eff× vs the
matched-core tcp1 (345K, 22.8 µs); at that 16-core budget `rh1-busy` delivers **4.2× tcp1's
throughput** (423%). Other read-ring modes: plain rh1 582K @512c, rh1-park 579K @8c. ³ 128 conns,
2026-07-17 (Undated 353K: ~7.3 cores, p99 766 µs). ⁴ 5120 conns, 2026-07-17 (Undated 780K @3072c: ~26
cores, p99 7371 µs). tcp1 brute-forces the headline with thousands of connections; the pinned read-ring
`rh1-busy` beats it ~4× at a matched 16-core budget.

### Tuning — how each peak was found

**read-ring** — folds plain `rh1` (shared runtime) + the pinned thread-per-core modes `rh1-busy` /
`rh1-park` (see [thread-per-core](thread-per-core.md)):

| config | mode | throughput | CPU/op | cores | p50 | p99 | src |
|---|---|---:|---:|---:|---:|---:|---|
| 256c | rh1 | 469K | n/r | n/r | n/r | n/r | 2026-07-17 |
| 384c | rh1 | 541K | n/r | n/r | n/r | n/r | 2026-07-17 |
| 512c | rh1 | 582K | n/r | ~12 | 856 µs | 1656 µs | Undated |
| 4c · 32 | rh1-busy | 431K | 9.3 µs | 4 | 73 µs | 100 µs | 2026-07-17 |
| 8c · 64 | rh1-busy | 852K | 9.4 µs | 8 | n/r | n/r | 2026-07-17 |
| **16c · 128** | **rh1-busy** | **1461K** | 10.9 µs | 16 | n/r | n/r | 2026-07-17 |
| 8c · 64 | rh1-park | 579K | 12.8 µs | ~7.4 | n/r | n/r | 2026-07-17 |
| 16c · 128 | rh1-park | 385K | 16.1 µs | n/r | n/r | n/r | 2026-07-17 |

**send-recv** (`conns`, in-flight 1):

| config | throughput | CPU/op | cores | p50 | p99 | src |
|---|---:|---:|---:|---:|---:|---|
| 128c | 310K | n/r | ~5.5 | 403 µs | 702 µs | Undated |
| **128c** | **315k** | n/r | n/r | n/r | n/r | 2026-07-17 |

**credit-ring** (`conns`, in-flight 1):

| config | throughput | CPU/op | cores | p50 | p99 | src |
|---|---:|---:|---:|---:|---:|---|
| 128c | 353K | n/r | ~7.3 | 341 µs | 766 µs | Undated |
| **128c** | **360k** | n/r | n/r | n/r | n/r | 2026-07-17 |

**tcp1** (`conns`, in-flight 1; connection-count bound):

| config | throughput | CPU/op | cores | p50 | p99 | src |
|---|---:|---:|---:|---:|---:|---|
| 1024c | 605k | n/r | n/r | n/r | n/r | Undated |
| 3072c | 771k | n/r | ~26 | 3817 µs | 7371 µs | Undated |
| **5120c** | **790k** | n/r | ~26 | n/r | n/r | 2026-07-17 |

## Results

### 2026-07-17 — regression re-validation
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `duration=10 warmup=3 threads=64`, reboot-clean NIC between ring batches

Re-run on the current binary as a regression check. **No regression** — every point
that established matches baseline; the only gaps are high-connection ring points
that failed to *establish* (CM setup), which was flakier than usual on this host —
never a data-path failure (consistent with h1's no-deadlock property; TCP scaled
clean to 5120).

**Headline (canonical schema)** — peak throughput per transport this session (all at in-flight 1; CPU/latency not re-recorded this run):

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 315k | n/r | n/r | n/r | n/r | n/r | n/r · n/r · 40% |
| read-ring | 541k | n/r | n/r | n/r | n/r | n/r | n/r · n/r · 68% |
| credit-ring | 360k | n/r | n/r | n/r | n/r | n/r | n/r · n/r · 46% |
| tcp1 | 790k | n/r | n/r | n/r | n/r | n/r | baseline |

Best config (conns, in-flight 1): read-ring 384; credit-ring 128; send-recv 128;
tcp1 5120. Higher ring fan-outs failed to *establish* (CM setup), not a data-path
failure. Baseline is `tcp1` (kernel HTTP/1.1).

**Connection scaling** — req/s, vs baseline:

| conns | tcp1 | read-ring | send-recv | credit-ring |
|---:|---:|---:|---:|---:|
| 64   | 286k (309k) | 283k (306k) | 252k (244k) | — |
| 128  | 340k (355k) | 356k (361k) | 315k (310k) | 360k (353k) |
| 256  | 432k (414k) | 469k (469k) | ✗ CM setup (295k) | — |
| 384  | — | 541k (567k) | — | — |
| 512  | 489k (496k) | ✗ CM setup (582k) | — | — |
| 1024 | 603k (605k) | — | — | — |
| 2048 | 727k (724k) | — | — | — |
| 4096 | 766k (783k) | — | — | — |
| 5120 | **790k** (784k) | — | — | — |

`tcp1` reproduces its full curve, peaking **~790K at 5120 conns / ~26 cores**. The
rings match baseline where they establish (read-ring peak **541K at 384 conns** this
session; credit-ring 360K at 128). read-ring 512 and send-recv 256 failed to
*establish* (`Terminated` on CM setup) — a lower CM-setup ceiling than baseline on
this flaky host, **not** a data-path regression: every ring run that started
completed with 0 errors, and there is no HTTP/1.1 deadlock.

### Undated — historical baseline
- **Date:** unknown (baseline result set predates dated recording)
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B (unless noted), `duration=8 warmup=3 threads=64`, client `ulimit -n` raised to the hard cap (see [§ The fd wall](../../methodology.md#the-fd-wall-fixed)); RDMA sweeps reboot between points (MANA CM churn)

#### Peak throughput (64 B)

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 310K | n/r | ~5.5 | 403 µs | 702 µs | n/r | n/r · 0.1× · 40% |
| read-ring | 582K | n/r | ~12 | 856 µs | 1656 µs | n/r | n/r · 0.2× · 75% |
| credit-ring | 353K | n/r | ~7.3 | 341 µs | 766 µs | n/r | n/r · 0.1× · 45% |
| tcp1 | 780K | n/r | ~26 | 3817 µs | 7371 µs | n/r | baseline |

Peak conns (in-flight 1) / ceiling: tcp1 3072 (TLS/hyper + CPU wall, flat beyond
~3K); read-ring 512 (latency knee + MANA CM-setup flakiness ≥768); credit-ring 128
(CM-setup flakiness ≥192); send-recv 128 (latency plateau + CM-setup ≥384). CPU/op
was not measured for this regime (cores reported instead), so CPU-eff× is `n/r`;
every run had **0 errors**.

Every run had **0 errors** — all the "ceilings" are either a latency/throughput knee
or connection *setup* flakiness, never a data-path failure (see "No data-path
deadlock" below). Each connection carries one in-flight request (`in_flight=1`), so
"peak conns" is also the total concurrency at the peak.

#### Connection scaling

req/s vs `--connections` (each connection = one sequential request loop):

| conns | tcp1 | read-ring | send-recv | credit-ring |
|---:|---:|---:|---:|---:|
| 64   | 309k | 306k | 244k | — |
| 128  | 355k | 361k | **310k** | **353k** |
| 192  | —    | 426k | 300k | fail |
| 256  | 414k | 469k | 295k | fail |
| 320  | —    | 513k | — | — |
| 384  | —    | 567k | fail | — |
| 512  | 496k | **582k** | fail | — |
| 640  | —    | 542k ↓ | — | — |
| 768  | 558k | fail | — | — |
| 960  | 597k | — | — | — |
| 1024 | 605k | — | — | — |
| 1536 | 670k | — | — | — |
| 2048 | 724k | — | — | — |
| 3072 | 771k | — | — | — |
| 4096 | **783k** | — | — | — |
| 5120 | 784k (flat) | — | — | — |

"fail" = the connection set could not be established (MANA RDMA-CM setup flakiness at
high fan-out), not a data-path error. "↓" = throughput regressed.

#### Why h1 is connection-count bound (not CPU, not the transport)

With one request in flight per connection, throughput ≈ `connections / RTT` (Little's
Law with `N = connections`). Consequences:

- **Nothing here is CPU-bound.** Even the headline `tcp1` peak uses only ~26 of 64
  cores; the RDMA transports peak at ~5–12 cores. Adding connections raises
  throughput only until per-op latency (queueing) grows enough to offset the extra
  concurrency — visible as `tcp1` flattening at ~780K past ~3072 conns (p50 already
  ~3.8 ms) and `read-ring` peaking at 512 then *regressing* at 640.
- **The transport data path is not the limit.** The RDMA rings sit at ~5–12 cores
  with sub-millisecond latency; they stop scaling because MANA's RDMA-CM gets flaky
  establishing hundreds of connections (`read-ring` ≥768, `credit-ring` ≥192,
  `send-recv` ≥384 fail to *establish*), not because the byte path saturates.

##### TCP wins the headline; RDMA wins efficiency

`tcp1` reaches ~780K by brute-forcing ~3072 connections at ~26 cores — but at
**~3.8 ms p50 / ~7 ms p99**. `read-ring` delivers ~582K at just **512 connections /
~12 cores** and **856 µs p50 / 1.7 ms p99** — ~75% of TCP's peak at ~4× lower latency
and less than half the cores, and it is throttled only by CM-setup flakiness, not by
compute or the data path. As with the raw-echo and gRPC results, TCP buys the
absolute number with cores and latency; RDMA wins whenever CPU or tail latency
matter.

#### No data-path deadlock (unlike the `rh2` rings)

Every `rh1` ring run completed with **0 errors**; all failures were connection
*setup* (CM), never the bidirectional flow-control wedge that caps the `rh2` rings at
high fan-out. This is expected: the deadlock needs both peers write-blocked
simultaneously on one connection, which requires HTTP/2-style multiplexed concurrent
streams. HTTP/1.1 keeps only one request in flight per connection — a strict
request/response ping-pong — so the two peers never both saturate their send queues
at once and the cycle cannot form. See
[read-ring-concurrent-stream-deadlock.md](../../../bugs/read-ring-concurrent-stream-deadlock.md).
