# Echo benchmark (`--mode echo`)

The raw-transport echo benchmark: a one-message request/response echo with **no
gRPC / TLS / HTTP-2**, isolating the transport itself. Each request is one
`send_copy` and each response one recv completion, so latency and throughput map
1:1 to transport behaviour. A matched raw-TCP echo (`--transport tcp`) gives an
apples-to-apples kernel baseline, and `--in-flight N` sweeps how many requests
each connection keeps outstanding (pipeline depth).

## Completion topologies

The same echo workload is measured under three completion-delivery topologies:

- **`echo`** (arm-park, default) — per-connection interrupt-armed CQs serviced by
  a shared multi-thread tokio runtime; work-steals across all vCPUs and parks
  when idle. Best deep-pipeline throughput and CPU/op.
- **`echo-busy`** (busy-poll) — `--threads N` pins N cores, each spinning one
  shared CQ at 100 % and demuxing completions by `qp_num`. Lowest latency in the
  shallow / request-response regime; burns every pinned core even when idle.
- **`echo-park`** (thread-per-core arm-park) — pinned `current_thread` cores with
  per-connection armed CQs that **park** in `epoll_wait` when idle (reactor-per-
  core, no work-stealing). The middle point: pinned locality with 0 % idle CPU.

## References

- **Design** (stack, request/response pipeline, poll loops, metrics, diagrams):
  [../../design/EchoBenchmark.md](../../design/EchoBenchmark.md)
- **How to run**: the `rdma-io-benchmarking` skill and `docs/Benchmark.md` in the
  `rdma-io-bench` repo.
- **Measured results** (data tables, grouped by regime — more added over time):
  - [message-rate (64 B), arm-park](message-rate-64b.md)
  - [large-payload (8 KiB), read-ring vs TCP](large-payload-8kib.md)
  - [busy-poll ceiling (`echo-busy`)](busy-poll.md)
  - [thread-per-core arm-park (`echo-park`)](thread-per-core-park.md)
- **Other scenarios**: [gRPC](../grpc/README.md) · [HTTP/1.1](../h1/README.md)
