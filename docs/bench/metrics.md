# Benchmark metrics

Definitions for the metrics reported across all scenarios and environments. Each
result block reports some subset of these; this doc is the single authoritative
definition for each.

## Throughput and latency

- **`req/s`** (throughput) — completed requests per second over the measured
  window (warm-up excluded).
- **`p50` / `p99`** — median and 99th-percentile per-request (or per-RPC) latency,
  in microseconds unless stated.
- **Gbps** — for large-payload regimes, application goodput = `req/s × payload`
  expressed in gigabits per second.

## CPU efficiency

The echo client samples its own resource use: a sampler task snapshots
`/proc/self/stat` (user + system CPU, summed across all threads) at the warm-up and
benchmark deadlines, so the delta covers **only the measured window** (excluding
connection setup and warm-up), plus the `VmHWM` peak RSS. These surface in the
result JSON as `cpu_seconds`, `cpu_us_per_op`, and `peak_rss_kb`.

> Why in-process, not `/usr/bin/time`? Whole-process CPU is dominated by the long
> ring connection setup (~60–80 s at 64 connections), which would swamp the ~10 s
> measured window. Sampling across the window is the accurate per-op cost.

All derived from the same client-side `cpu_seconds` = user + kernel CPU-time
consumed by every thread during the window:

- **`cores busy`** = `cpu_seconds / duration_secs`. The *average* number of
  fully-saturated cores the client used — e.g. 94 CPU-seconds over a 10 s window =
  9.4 cores. A time-average (not a peak) of compute-equivalent cores spread by the
  scheduler across the vCPUs, not pinned cores. The ceiling is the vCPU count. It
  counts **user + kernel** time, so TCP's figure is inflated by in-kernel `stime`
  (syscalls, softirq, TCP/IP stack) while RDMA is almost all user-space `utime`
  (kernel-bypass).
- **`CPU/op µs`** (`cpu_us_per_op`) = `cpu_seconds / total_requests × 1e6` — the
  same CPU-time divided by work done instead of by wall-time.
- **`peak RSS`** (`peak_rss_kb`, reported as MB in tables) — `VmHWM`, the peak
  resident set; captures the registered-buffer / MR cost of the RDMA transports.

Both CPU figures are **client-side only**; the server does comparable work but is
not sampled.
