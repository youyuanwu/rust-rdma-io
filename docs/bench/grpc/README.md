# gRPC-over-RDMA benchmark (`rh2`)

The tonic gRPC benchmark: a Greeter unary RPC driven over the full **TLS 1.3 +
HTTP/2 + protobuf + hyper** stack on top of the RDMA byte stream
(`AsyncRdmaStream`), versus a `tcp` baseline running the *same* tonic + TLS stack
over a kernel socket. This measures gRPC-over-RDMA end to end — the layers gRPC
adds on top of the transport, not the raw transport (for that see the
[echo](../echo/README.md) scenario).

- **`rh2`** — gRPC / HTTP-2 over one of the RDMA transports (`send-recv` /
  `read-ring` / `credit-ring`); MR-rkey fallback is auto-detected per run.
- **`rh3`** — the QUIC / HTTP-3 variant (quinn over `RdmaUdpSocket`).
- **`tcp`** — the kernel-socket baseline (same tonic + TLS stack).

HTTP/2 multiplexes many concurrent RPCs over one connection, so both
`--connections` and `--in-flight` (streams per connection) drive offered load.
This is where the high-fan-out **flow-control deadlock** in the
`AsyncRdmaStream` / HTTP-2 layer surfaces — see the results and
[../../bugs/read-ring-concurrent-stream-deadlock.md](../../bugs/read-ring-concurrent-stream-deadlock.md).

## References

- **How to run** (design, infrastructure, inventory): `docs/Benchmark.md` in the
  `rdma-io-bench` repo; run/interpretation guidance in the `rdma-io-benchmarking`
  skill.
- **Measured results** (data tables, grouped by regime — more added over time):
  - [throughput & pipelining (64 B)](throughput-64b.md)
  - [payload size (64 B → 8 KiB)](payload.md)
  - [client CPU & memory](cpu-memory.md)
- **Other scenarios**: [echo](../echo/README.md) · [HTTP/1.1](../h1/README.md)
