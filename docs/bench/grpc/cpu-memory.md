# gRPC client CPU & memory

Client-side CPU per request and peak RSS across pipelining depth and full scale,
plus the overall takeaways. See [README.md](README.md) for the scenario. Other
gRPC datasets: [throughput & pipelining (64 B)](throughput-64b.md) ·
[payload size](payload.md).

## Client CPU & memory

Client-side CPU per request and peak RSS, measured over the benchmark window
(connection setup and warmup excluded), 8 conns / 8 threads, 64 B:

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
because batching amortizes the per-request stack overhead over more work per
wakeup. RDMA carries a higher peak RSS than TCP (registered buffer pools / MRs),
and `send-recv` the most — a full recv-buffer pool per connection.

At full scale (64 conns / 64 threads, `in_flight=1`, ~17–19 client cores) the
picture is unchanged — CPU/op stays transport-agnostic:

| metric | send-recv | read-ring | credit-ring | tcp |
|---|---:|---:|---:|---:|
| throughput req/s | 191.9K | 255.6K | 237.9K | 220.6K |
| CPU µs/op | 70.8 | 75.5 | 75.2 | 75.4 |
| peak RSS MB | 84 | 52 | 52 | 38 |

This is the sharpest contrast with raw `echo`, which at 64×64 measures `read-ring`
at **1.2 µs/op vs TCP 5.4 µs/op** (RDMA ~4× cheaper): at the gRPC layer that entire
efficiency gap is consumed by the TLS/HTTP-2/protobuf stack, not the transport.

**Takeaways.** With the current data path (native-tokio + vectored `send_gather`)
`rh2` gRPC-over-RDMA now **meets or beats the TCP baseline at every tested
config** (the old ~40K rh2 ceiling is gone). The `read-ring` transport pipelines
cleanly at depth after the bidirectional-deadlock fix
([read-ring-concurrent-stream-deadlock.md](../../bugs/read-ring-concurrent-stream-deadlock.md)).
RDMA wins on throughput *and* latency up to ~200 connections (peak ~365–376K
req/s, transport-dependent); beyond that a high-concurrency flow-control deadlock
appears (all connections still establish — it is not a connection-setup ceiling),
so TCP, which keeps scaling, is the choice for very high connection fan-out until
that deadlock is root-caused.
