# HTTP/1.1 large-payload (8 KiB)

Bandwidth-bound 8 KiB throughput and connection scaling for `rh1` vs `tcp1`. See
[README.md](README.md) for the scenario. Other HTTP/1.1 datasets:
[throughput (64 B)](throughput-64b.md) · [thread-per-core](thread-per-core.md) ·
[methodology](methodology.md).

## Large-payload (8 KiB) ceiling

At 8 KiB the workload becomes **bandwidth-bound** rather than message-rate bound.
The ring transports carry each request/response in a single RDMA message by
setting `ring_max_msg=9216` (> 8192 body + HTTP/1.1 headers + TLS record
overhead; still 7 ring slots) — the same max-message strategy used for the 8 KiB
echo and gRPC runs. `send-recv` (64 KiB stream buffer) and `tcp1` (kernel) need
no tuning.

| mode / transport | peak throughput | bandwidth | peak conns | ~cores | p50 | p99 | ceiling set by |
|---|---:|---:|---:|---:|---:|---:|---|
| **tcp1** (kernel) | **~437K** | **28.6 Gbps** | 1024 | ~19 | 2309 µs | 4247 µs | bandwidth (flat past 1024) |
| **rh1** read-ring | ~410K | 26.9 Gbps | 384 | ~14 | 889 µs | 1954 µs | MANA CM-setup flakiness (≥512) |
| **rh1** credit-ring | ~314K | 20.6 Gbps | 128 | ~10 | 383 µs | 874 µs | MANA CM-setup flakiness (≥192) |
| **rh1** send-recv | ~308K | 20.2 Gbps | 256 | ~8 | 819 µs | 1380 µs | latency plateau + CM flakiness |

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

- **Bandwidth-bound, ~20–29 Gbps.** `tcp1` wins the headline bandwidth
  (~28.6 Gbps) by scaling to ~1024 connections at ~19 cores; **read-ring is close
  behind (~26.9 Gbps) at just 384 connections / ~14 cores** — again more
  CPU-efficient and ~2–3× lower latency (p50 889 µs vs `tcp1` 2325 µs), and capped
  only by MANA CM-setup flakiness at ≥512, not the data path.
- **Still no deadlock.** As at 64 B, every ring ran 0 errors; the rings stop at
  connection *setup* (CM), never the `rh2` bidirectional wedge — h1's
  one-request-per-connection keeps the peers from both write-blocking. This is the
  **opposite** of the 8 KiB *gRPC* result, where the `rh2` rings hit the
  flow-control deadlock at modest fan-out and TCP won decisively (see
  [gRPC](../grpc/payload.md)).
- **read-ring trails raw echo.** h1 read-ring peaks ~26.9 Gbps vs raw-`echo`
  read-ring's ~36 Gbps at 8 KiB: the TLS + hyper + HTTP/1.1 stack (~34 µs/op) plus
  one-in-flight-per-connection request/response cap it below the raw transport's
  zero-copy bandwidth.
