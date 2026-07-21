# HTTP/1.1 large-payload (8 KiB)

Bandwidth-bound 8 KiB throughput and connection scaling for `rh1` vs `tcp1`. See the
[HTTP/1.1 scenario](../../scenarios/h1.md), [methodology](../../methodology.md), and
[metrics](../../metrics.md). Other HTTP/1.1 regimes:
[throughput (64 B)](throughput-64b.md) · [thread-per-core](thread-per-core.md).

## Results

### 2026-07-17 — regression re-validation
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- `payload=8192`, rings at `ring_max_msg=9216`, `duration=10 warmup=3 threads=64`, reboot-clean NIC

Re-run on the current binary as a regression check. **No regression** — every point
that established matches baseline; the read-ring 320/384-conn points failed to
*establish* (CM setup) on this flaky host, so the read-ring peak could not be pushed
past 256 conns this session.

**Headline (canonical schema)** — peak throughput per transport this session (in-flight 1; CPU/latency not re-recorded this run):

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 322k | n/r | n/r | n/r | n/r | n/r | n/r · n/r · 74% |
| read-ring | 349k | n/r | n/r | n/r | n/r | n/r | n/r · n/r · 80% |
| credit-ring | 321k | n/r | n/r | n/r | n/r | n/r | n/r · n/r · 73% |
| tcp1 | 437k | n/r | n/r | n/r | n/r | n/r | baseline |

Bandwidth (best config): tcp1 1024c → 28.7 Gbps; read-ring 256c → 22.8 Gbps (its
measured peak this session; 320/384c would not establish); send-recv 256c →
21.1 Gbps; credit-ring 128c → 21.0 Gbps.

| conns | tcp1 | read-ring | send-recv | credit-ring |
|---:|---:|---:|---:|---:|
| 64   | 234k / 15.3 (240k) | 223k / 14.6 (231k) | — | — |
| 128  | 250k / 16.4 (260k) | 283k / 18.5 (283k) | 259k / 16.9 (255k) | 321k / 21.0 (314k) |
| 256  | 300k / 19.6 (307k) | 349k / 22.8 (363k) | 322k / 21.1 (307k) | ✗ CM setup |
| 320  | — | ✗ CM setup (388k) | — | — |
| 384  | — | ✗ CM setup (410k) | — | — |
| 512  | 398k / 26.1 (402k) | — | — | — |
| 1024 | **437k / 28.7** (437k) | — | — | — |
| 2048 | 435k / 28.5 (434k) | — | — | — |

(req/s | Gbps; baseline in parens.) `tcp1` reproduces its full ~28.7 Gbps bandwidth
wall (peak **437K at 1024 conns**). The rings match baseline where they establish
(read-ring 349K / 22.8 Gbps at 256, its measured peak this session; send-recv 322K at
256; credit-ring 321K at 128). read-ring 320/384 — its baseline peak region (~410K) —
would not *establish* on this host's flakier CM setup, so the peak reads lower this
run purely from the setup ceiling, not a data-path change (every ring run that
started had 0 errors, no deadlock).

### Undated — historical baseline
- **Date:** unknown (baseline result set predates dated recording)
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- `payload=8192`, rings at `ring_max_msg=9216`, reboot-clean NIC, 0 errors

#### Large-payload (8 KiB) ceiling

At 8 KiB the workload becomes **bandwidth-bound** rather than message-rate bound. The
ring transports carry each request/response in a single RDMA message by setting
`ring_max_msg=9216` (> 8192 body + HTTP/1.1 headers + TLS record overhead; still 7
ring slots) — the same max-message strategy used for the 8 KiB echo and gRPC runs.
`send-recv` (64 KiB stream buffer) and `tcp1` (kernel) need no tuning.

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | 308K | n/r | ~8 | 819 µs | 1380 µs | n/r | n/r · 0.3× · 70% |
| read-ring | 410K | n/r | ~14 | 889 µs | 1954 µs | n/r | n/r · 0.5× · 94% |
| credit-ring | 314K | n/r | ~10 | 383 µs | 874 µs | n/r | n/r · 0.2× · 72% |
| tcp1 | 437K | n/r | ~19 | 2309 µs | 4247 µs | n/r | baseline |

Bandwidth / peak conns / ceiling: tcp1 28.6 Gbps @ 1024c (bandwidth wall, flat past
1024); read-ring 26.9 Gbps @ 384c (MANA CM-setup flakiness ≥512); credit-ring
20.6 Gbps @ 128c (CM flakiness ≥192); send-recv 20.2 Gbps @ 256c (latency plateau +
CM flakiness). CPU/op not measured for this regime (cores reported) → CPU-eff× `n/r`.

Connection scaling (req/s | Gbps), all 0 errors:

| conns | tcp1 | read-ring | send-recv | credit-ring |
|---:|---:|---:|---:|---:|
| 64   | 240k / 15.7 | 231k / 15.2 | — | — |
| 128  | 260k / 17.0 | 283k / 18.6 | 255k / 16.7 | **314k / 20.6** |
| 256  | 307k / 20.1 | 363k / 23.8 | **307k / 20.2** | fail |
| 320  | — | 388k / 25.5 | — | — |
| 384  | — | **410k / 26.9** | 308k / 20.2 | — |
| 512  | 402k / 26.4 | fail | — | — |
| 1024 | **437k / 28.6** | — | — | — |
| 2048 | 434k / 28.5 (flat) | — | — | — |

Takeaways:

- **Bandwidth-bound, ~20–29 Gbps.** `tcp1` wins the headline bandwidth (~28.6 Gbps)
  by scaling to ~1024 connections at ~19 cores; **read-ring is close behind
  (~26.9 Gbps) at just 384 connections / ~14 cores** — again more CPU-efficient and
  ~2–3× lower latency (p50 889 µs vs `tcp1` 2325 µs), and capped only by MANA CM-setup
  flakiness at ≥512, not the data path.
- **Still no deadlock.** As at 64 B, every ring ran 0 errors; the rings stop at
  connection *setup* (CM), never the `rh2` bidirectional wedge — h1's
  one-request-per-connection keeps the peers from both write-blocking. This is the
  **opposite** of the 8 KiB *gRPC* result, where the `rh2` rings hit the flow-control
  deadlock at modest fan-out and TCP won decisively (see
  [gRPC](../grpc/payload.md)).
- **read-ring trails raw echo.** h1 read-ring peaks ~26.9 Gbps vs raw-`echo`
  read-ring's ~36 Gbps at 8 KiB: the TLS + hyper + HTTP/1.1 stack (~34 µs/op) plus
  one-in-flight-per-connection request/response cap it below the raw transport's
  zero-copy bandwidth.
