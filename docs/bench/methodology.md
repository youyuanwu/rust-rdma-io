# Benchmark methodology

How the benchmarks are run — the fd-limit fix, run parameters, reboot cadence, and
`TCP_NODELAY` — shared by every scenario and environment. For the design and the
how-to-run tooling see [../design/EchoBenchmark.md](../design/EchoBenchmark.md),
`docs/Benchmark.md` in the `rdma-io-bench` repo, and the `rdma-io-benchmarking`
skill (run and interpretation guidance).

## The fd wall (fixed)

Each RDMA connection uses ~3 fds (CM id + QP + event channel) and TCP ~1, so at
high connection counts the client would otherwise hit the distro-default soft
`ulimit -n = 1024`. The hard limit is already 1048576, so the bench playbooks
raise the soft limit to match — the connection counts in these docs are therefore
**not** fd-bound:

- **`tests/e2e/playbooks/setup_rdma_hw.yml`** writes
  `/etc/security/limits.d/99-rdma-bench.conf` (persistent soft+hard `nofile`).
- **`tests/e2e/playbooks/bench_run.yml`** runs `ulimit -n "$(ulimit -Hn)"` before
  launching the client and server (guaranteed at launch, independent of
  pam_limits on the non-interactive Ansible session).

## Run parameters

64 B message-rate runs use `duration=8 warmup=3 threads=64` unless a result block
states otherwise (large-payload and thread-per-core sweeps vary these — each block
records its own parameters). Peaks reported in the results are the **last clean
point** before throughput regresses or CM setup starts failing.

## Reboot cadence and NIC wedges

RDMA runs reboot between sweeps (`just reboot && just prepare-rdma`) because
back-to-back ring runs progressively wedge the MANA NIC's RDMA-CM. A `fail`/timeout
on a churned NIC is a **setup wedge, not a code result** — re-run isolated on a
fresh NIC before concluding. High-fan-out CM-setup flakiness (probabilistic beyond
a few hundred connections) is likewise a MANA RDMA-CM property, not a data-path
wall. See the `rdma-io-benchmarking` skill for the full run methodology.

## TCP_NODELAY

The TCP baselines (`tcp1` client + server, and the gRPC `tcp` server) run with
**`TCP_NODELAY`** (Nagle disabled), matching the `echo`/`rh1` paths so the
comparison against RDMA (which has no Nagle) is fair. Measured effect on these
numbers: **none** (within run-to-run noise) — the workload keeps one request in
flight per connection, so a request is never queued behind unacked small data and
Nagle's trigger condition is never met. It is kept as the correct low-latency
setting (and it matters for configs that emit many small writes per message, e.g.
HTTP/1.1 pipelining).
