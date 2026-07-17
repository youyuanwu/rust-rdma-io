# HTTP/1.1 benchmark methodology

The client fd-limit (`nofile`) fix and the run methodology — payload, durations,
reboot cadence, and TCP_NODELAY — shared by the HTTP/1.1 datasets. See
[README.md](README.md) for the scenario. Other HTTP/1.1 datasets:
[throughput (64 B)](throughput-64b.md) · [thread-per-core](thread-per-core.md) ·
[large-payload (8 KiB)](large-payload-8kib.md).

## The fd wall (fixed)

Each RDMA connection uses ~3 fds (CM id + QP + event channel) and TCP ~1, so at
high connection counts the client would otherwise hit the distro-default soft
`ulimit -n = 1024`. The hard limit is already 1048576, so the bench playbooks
raise the soft limit to match — the connection counts in this doc are therefore
**not** fd-bound:

- **`tests/e2e/playbooks/setup_rdma_hw.yml`** writes
  `/etc/security/limits.d/99-rdma-bench.conf` (persistent soft+hard `nofile`).
- **`tests/e2e/playbooks/bench_run.yml`** runs `ulimit -n "$(ulimit -Hn)"` before
  launching the client and server (guaranteed at launch, independent of
  pam_limits on the non-interactive Ansible session).

## Methodology

64 B payload, `duration=8 warmup=3 threads=64`. Peaks are the last clean point
before throughput regresses or CM setup starts failing. RDMA runs reboot between
sweeps (`just reboot && just prepare-rdma`) because back-to-back ring runs
progressively wedge the MANA NIC's RDMA-CM. A `fail`/timeout on a churned NIC is
a setup wedge, not a code result — re-run isolated on a fresh NIC before
concluding. See the `rdma-io-benchmarking` skill for the full run methodology.

The TCP baselines (`tcp1` client + server, and the gRPC `tcp` server) run with
**`TCP_NODELAY`** (Nagle disabled), matching the `echo`/`rh1` paths so the
comparison against RDMA (which has no Nagle) is fair. Measured effect on these
numbers: **none** (within run-to-run noise) — the workload keeps one request in
flight per connection, so a request is never queued behind unacked small data and
Nagle's trigger condition is never met. It is kept as the correct low-latency
setting (and it matters for configs that emit many small writes per message, e.g.
HTTP/1.1 pipelining).
