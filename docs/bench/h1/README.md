# HTTP/1.1 benchmark (`rh1` / `tcp1`)

The HTTP/1.1 benchmark drives the hyper `http1` + OpenSSL/TLS stack with **no
stream multiplexing** — each connection issues exactly one request at a time
(`--in-flight` is ignored and reported as 1), so offered load scales with
`--connections`. This is the structural opposite of the [gRPC](../grpc/README.md)
`rh2` scenario, where one connection multiplexes many concurrent RPCs.

- **`rh1`** — HTTP/1.1 over the same OpenSSL/TLS layer as `rh2`, carried on the
  raw RDMA byte stream (`send-recv` / `read-ring` / `credit-ring`) on a shared
  multi-thread tokio runtime.
- **`tcp1`** — the same hyper `http1` + OpenSSL stack over a kernel TCP socket
  (the non-RDMA baseline).

## Thread-per-core variants

`rh1-busy` and `rh1-park` pin each connection to a core for life — the read-ring
transport, `AsyncRdmaStream`, the OpenSSL session, and the hyper `http1` driver
all run on one core's `current_thread` runtime (mirroring the
[`echo-busy` / `echo-park`](../echo/README.md) topologies):

- **`rh1-busy`** — shared-CQ busy-poll `CoreDriver` per core (100 % core even
  when idle, no hot-path syscalls, lowest latency).
- **`rh1-park`** — per-connection interrupt-armed CQ; the core **parks** in
  `epoll_wait` when idle (0 % idle CPU), reactor-per-core with no work-stealing.

`--threads` is the pinned-core count; connections shard round-robin across them.

## References

- **How to run**: the `rdma-io-benchmarking` skill and `docs/Benchmark.md` in the
  `rdma-io-bench` repo.
- **Measured results** (data tables, grouped by regime — more added over time):
  - [throughput (64 B)](throughput-64b.md)
  - [thread-per-core (`rh1-busy` / `rh1-park`)](thread-per-core.md)
  - [large-payload (8 KiB)](large-payload-8kib.md)
  - [methodology](methodology.md)
- **Other scenarios**: [echo](../echo/README.md) · [gRPC](../grpc/README.md)
