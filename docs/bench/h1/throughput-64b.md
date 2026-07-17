# HTTP/1.1 throughput (64 B)

Peak throughput and connection scaling at 64 B, why HTTP/1.1 is connection-count
bound (not CPU or transport), and why the ring transports show no data-path
deadlock. See [README.md](README.md) for the scenario. Other HTTP/1.1 datasets:
[thread-per-core](thread-per-core.md) ·
[large-payload (8 KiB)](large-payload-8kib.md) · [methodology](methodology.md).

All runs: 64 B payload (unless noted — see "Large-payload (8 KiB) ceiling"),
`duration=8 warmup=3 threads=64`, on the Azure MANA RoCEv2 VMs, with the client
`ulimit -n` raised to the hard cap (1048576) — see "The fd wall (fixed)" below.
RDMA sweeps reboot between points (MANA CM churn).

## Peak throughput (64 B)

| mode / transport | peak throughput | peak conns | ~cores | p50 | p99 | ceiling set by |
|---|---:|---:|---:|---:|---:|---|
| **tcp1** (kernel) | **~780K** | 3072 | ~26 | 3817 µs | 7371 µs | TLS/hyper stack + CPU (flat beyond ~3K conns) |
| **rh1** read-ring | ~582K | 512 | ~12 | 856 µs | 1656 µs | latency knee + MANA CM-setup flakiness (≥768) |
| **rh1** credit-ring | ~353K | 128 | ~7.3 | 341 µs | 766 µs | MANA CM-setup flakiness (≥192) |
| **rh1** send-recv | ~310K | 128 | ~5.5 | 403 µs | 702 µs | latency plateau + CM-setup flakiness (≥384) |

Every run had **0 errors** — all the "ceilings" are either a latency/throughput
knee or connection *setup* flakiness, never a data-path failure (see "No
data-path deadlock" below). Each connection carries one in-flight request
(`in_flight=1`), so "peak conns" is also the total concurrency at the peak.

## Connection scaling

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

"fail" = the connection set could not be established (MANA RDMA-CM setup
flakiness at high fan-out), not a data-path error. "↓" = throughput regressed.

## Why h1 is connection-count bound (not CPU, not the transport)

With one request in flight per connection, throughput ≈ `connections / RTT`
(Little's Law with `N = connections`). Consequences:

- **Nothing here is CPU-bound.** Even the headline `tcp1` peak uses only ~26 of
  64 cores; the RDMA transports peak at ~5–12 cores. Adding connections raises
  throughput only until per-op latency (queueing) grows enough to offset the
  extra concurrency — visible as `tcp1` flattening at ~780K past ~3072 conns
  (p50 already ~3.8 ms) and `read-ring` peaking at 512 then *regressing* at 640.
- **The transport data path is not the limit.** The RDMA rings sit at ~5–12
  cores with sub-millisecond latency; they stop scaling because MANA's RDMA-CM
  gets flaky establishing hundreds of connections (`read-ring` ≥768,
  `credit-ring` ≥192, `send-recv` ≥384 fail to *establish*), not because the
  byte path saturates.

### TCP wins the headline; RDMA wins efficiency

`tcp1` reaches ~780K by brute-forcing ~3072 connections at ~26 cores — but at
**~3.8 ms p50 / ~7 ms p99**. `read-ring` delivers ~582K at just **512
connections / ~12 cores** and **856 µs p50 / 1.7 ms p99** — ~75% of TCP's peak
at ~4× lower latency and less than half the cores, and it is throttled only by
CM-setup flakiness, not by compute or the data path. As with the raw-echo and
gRPC results, TCP buys the absolute number with cores and latency; RDMA wins
whenever CPU or tail latency matter.

## No data-path deadlock (unlike the `rh2` rings)

Every `rh1` ring run completed with **0 errors**; all failures were connection
*setup* (CM), never the bidirectional flow-control wedge that caps the `rh2`
rings at high fan-out. This is expected: the deadlock needs both peers
write-blocked simultaneously on one connection, which requires HTTP/2-style
multiplexed concurrent streams. HTTP/1.1 keeps only one request in flight per
connection — a strict request/response ping-pong — so the two peers never both
saturate their send queues at once and the cycle cannot form. See
[../bugs/read-ring-concurrent-stream-deadlock.md](../../bugs/read-ring-concurrent-stream-deadlock.md).
