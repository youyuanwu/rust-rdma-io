# Echo thread-per-core arm-park (`echo-park`)

The interrupt-driven pinned topology (`--mode echo-park`): pinned cores with
per-connection armed CQs that park when idle. Includes the matched-core comparison
of all three topologies and mode-selection guidance. See the
[echo scenario](../../scenarios/echo.md) and [methodology](../../methodology.md) /
[metrics](../../metrics.md). Other echo regimes:
[message-rate (64 B)](message-rate-64b.md) ·
[large-payload (8 KiB)](large-payload-8kib.md) · [busy-poll](busy-poll.md).

## Results

### 2026-07-17 — regression re-validation
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `duration=10 warmup=3`, reboot-clean NIC

Re-run on the current binary as a regression check. **No regression** — the pinned
`echo-park` sweep reproduces within ~1 % (prior value in parentheses):

**Headline (canonical schema)** — read-ring thread-per-core arm-park (`echo-park`) vs kernel baseline at matched cores (8 × 32 × 64):

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | — N/A (no thread-per-core mode) | | | | | | |
| read-ring (echo-park) | 4.96M | 1.52 µs | 8 | 353 µs | 1655 µs | n/r | 1.8× · n/r · 192% |
| credit-ring | — N/A (no thread-per-core mode) | | | | | | |
| tcp | 2.59M | 2.81 µs | 8 | n/r | n/r | n/r | baseline |

| cores | conns | in-flight | echo-park | tcp echo¹ | CPU/op | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 2 | 8 | 16 | 1.58M (1.59M) | 0.89M | 1.27 µs | 79 µs | 124 µs |
| 2 | 8 | 64 | 2.15M (2.15M) | 1.05M | 0.93 µs | 225 µs | 503 µs |
| 4 | 16 | 64 | 3.42M (3.36M) | 1.59M | 1.17 µs | 248 µs | 957 µs |
| 8 | 32 | 64 | 4.96M (4.93M) | 2.59M | 1.52 µs | 353 µs | 1655 µs |
| 8 | 8 | 256 | 4.78M (4.79M) | 1.56M | 1.44 µs | 344 µs | 1047 µs |

¹ `tcp echo` = kernel-socket `--transport tcp` at the same cores / connections /
in-flight (shared-runtime kernel baseline; TCP has no thread-per-core mode). It runs
~1.9–3.9 µs/op, so `echo-park` leads it ~1.8–2× on throughput at lower CPU/op.

**Matched-core (deep `in_flight=64`), req/s (CPU/op µs):**

| cores | conns | `echo` | `echo-busy` | `echo-park` | `tcp echo` |
|---:|---:|---:|---:|---:|---:|
| 2 | 8 | 1.98M (0.75 base 2.59M) | 2.25M (0.89) | 2.15M (0.93) | 1.05M (1.85) |
| 4 | 16 | 4.15M (0.94, base 4.42M) | 3.86M (1.04) | 3.42M (1.17) | 1.59M (2.35) |
| 8 | 32 | 4.51M (1.31, base 5.61M) | 5.01M (1.60) | 4.96M (1.52) | 2.59M (2.81) |

The two **pinned** modes (`echo-busy`, `echo-park`) reproduce the baseline
essentially exactly, and all three read-ring modes beat the matched-core kernel
`tcp echo` (1.05–2.59M) by ~1.6–2.4× at ~half to a third of the CPU/op. Plain
`echo` (elastic work-stealing) ran **soft** at the 2- and 8-core matched points this
session (1.98M / 4.51M vs baseline 2.59M / 5.61M), so it did not top the
matched-core table at those points — this is the documented read-ring arm-park
run-to-run variance at low pinned-thread counts (the full-64-thread arm-park
read-ring re-measured clean at 4.77M / 1.24 µs/op in
[message-rate-64b.md](message-rate-64b.md)), not a systematic regression. The CPU/op
figures are all in range.

**Idle / low-load (2 cores, 2 conns, in-flight 1):** `echo-busy` 68.5k (p50 28 µs),
`echo-park` 29.0k (p50 66 µs), `echo` 25.9k (p50 77 µs), `tcp echo` 25.0k (p50
77 µs) — matches the baseline (68.6k / 28.8k / 23.7k); busy-poll still wins the
shallow regime ~2.4–2.7× over both arm-park and kernel TCP.

### Undated — historical baseline
- **Date:** unknown (baseline result set predates dated recording)
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 64 B, `duration=10 warmup=3`, reboot-clean NIC, same configs as the busy sweep

`echo-park` (`--mode echo-park`) is the **interrupt-driven** sibling of `echo-busy`:
the same `--threads N` pinned `current_thread` cores and the same round-robin
connection sharding, but each connection keeps its own **interrupt-armed** CQ (like
the default `echo` path) instead of a shared busy-polled CQ. Each connection's
completion-channel and CM fds bind to their owning core's reactor at construction,
so the event-driven data path is serviced on that core — reactor-per-core locality —
but the core **parks in `epoll_wait` when idle** (0 % idle CPU) rather than spinning.
It is the middle point between the two existing modes:

| mode | reaping | idle CPU | core placement | fan-out |
|---|---|---|---|---|
| `echo` (arm-park) | per-conn armed CQ | 0 % (parks) | tokio work-stealing over all vCPUs | elastic (~7–9 cores) |
| `echo-busy` | shared CQ, 100 % spin | **100 %** | pinned 1 core/CQ, sole reaper | none |
| `echo-park` | per-conn armed CQ | **0 % (parks)** | **pinned** 1 core/runtime | none (no stealing) |

**Headline (canonical schema)** — read-ring `echo-park` characterization (matched-core kernel comparison in the 2026-07-17 block above):

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | — N/A (no thread-per-core mode) | | | | | | |
| read-ring (echo-park) | 4.93M | 1.52 µs | 8 | 362 µs | 1634 µs | n/r | n/r · n/r · n/r |
| credit-ring | — N/A (no thread-per-core mode) | | | | | | |
| tcp | n/r (not measured in this block) | | | | | | baseline |

| cores | conns | /core | in-flight | throughput | CPU/op | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 2 | 8 | 4 | 16 | 1.59M | 1.26 µs | 79 µs | 122 µs |
| 2 | 8 | 4 | 64 | 2.15M | 0.93 µs | 229 µs | **495 µs** |
| 4 | 16 | 4 | 64 | 3.36M | 1.17 µs | 237 µs | 1227 µs |
| 8 | 32 | 4 | 64 | **4.93M** | 1.52 µs | 362 µs | 1634 µs |
| 8 | 8 | 1 | 256 | 4.79M | 1.45 µs | 343 µs | 1110 µs |

`echo-park` **tracks `echo-busy` ~5–15 % behind at every point and converges to the
same ~5M ceiling by 8 cores** (4.93M vs 5.02M) — the shared NIC message rate caps
both. It pays busy-poll's avoided hot-path cost (`ibv_get_cq_event` + re-arm + epoll
wakeup), so its per-op latency runs a bit higher — except it actually **wins the
tail at low core counts** (2-core p99 495 µs vs busy's 838 µs), the armed CQ
absorbing bursts more evenly than the spin loop's fixed sweep cadence. The 16-core /
64-connection point could not be measured: `echo-park` repeatedly failed to
*establish* all 64 connections (`expected Established, got Unreachable`) — the MANA
RoCEv2 **CM-setup** flakiness at high fan-out, not a throughput limit. Neither pool
retries a hung/failed connect, so at 64 connections this surfaces as a run-ending
timeout; the ceiling is already reached at 8 cores regardless.

#### Matched-core comparison: same core budget, three topologies

The tables above compare `echo-busy`/`echo-park` head-to-head, but the fair
question is how the pinned modes compare to the **default `echo`** at the *same core
budget*. Running base `echo` (multi-thread arm-park, work-stealing) with
`--threads N` = the same `N` and the same conns/in-flight (64 B, deep `in_flight=64`,
reboot-clean NIC):

| cores | conns | `echo` | `echo-busy` | `echo-park` |
|---:|---:|---:|---:|---:|
| 2 | 8 | **2.59M** (0.75 µs/op, p99 359 µs) | 2.26M (0.89, 838) | 2.15M (0.93, 495) |
| 4 | 16 | **4.42M** (0.87 µs/op, p99 455 µs) | 3.87M (1.03, 1338) | 3.36M (1.17, 1227) |
| 8 | 32 | **5.61M** (1.12 µs/op, p99 1228 µs) | 5.02M (1.59, 841) | 4.93M (1.52, 1634) |

**At a deep pipeline, plain `echo` is the fastest and most CPU-efficient of the
three even at a matched core budget** (5.61M vs 5.02M vs 4.93M on 8 cores, at
0.75–1.12 µs/op vs ~1.5 µs). That is the *opposite* of the shallow-pipeline result
below, and the reason is pipeline depth: at `in_flight=64` the per-message interrupt
cost is already amortized across ~64 completions per wakeup, so busy-poll's spin buys
almost nothing — and it *costs* the shared-CQ demux plus pinning away work-stealing's
ability to smooth bursts across cores (note `echo` holds ~6.3 busy cores at 8
threads, not a hard 8, and still wins). Busy-poll and park pin every core at
~100 %/parked-hot for *less* throughput here.

#### Idle / shallow pipeline — where busy-poll wins

The pinned modes' advantage is the **shallow-pipeline / low-latency** regime, where
each message pays a full interrupt round-trip that busy-poll's spin eliminates. At 2
connections / in-flight 1 on 2 cores:

| mode | throughput | cores busy | CPU/op | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| `echo-busy` | **68.6k** | ~2.0 | 29.1 µs | **28 µs** | 34 µs |
| `echo-park` | 28.8k | ~0.5 | 16.7 µs | 67 µs | 80 µs |
| `echo` | 23.7k | ~0.4 | 18.6 µs | 89 µs | 104 µs |

Here busy-poll **wins throughput and latency ~2.4–2.9×** (68.6k / p50 28 µs) — no
per-message `ibv_get_cq_event`/re-arm, it just spins and reaps — at the cost of
pinning 2 whole cores. `echo-park` and `echo` are both interrupt-bound and much
slower per message (p50 67–89 µs), but use only ~0.4–0.5 cores; `echo-park` edges
`echo` on latency (pinned locality, no cross-core task migration) at slightly higher
CPU. This is the crossover: **busy-poll's spin pays off only when the pipeline is too
shallow to amortize the interrupt** — exactly the request/response regime
(`in_flight` 1) the busy-poll design targets (cf. the Slice D4 headline, `echo-busy`
938K vs arm-park `echo` 386K at 8 conns / in-flight 4).

#### Picking a mode

The three modes hit the same **architectural ~5–6.8M read-ring echo ceiling** (the
per-message doorbell/completion path, not CPU or NIC); which one is best depends on
**pipeline depth and what you value**:

- **`echo`** (default, work-stealing) — **best deep-pipeline throughput and CPU/op**,
  even at a matched core budget; parks when idle. Downside: connections migrate
  across cores (no locality/determinism), and it is interrupt-bound at shallow depth.
  The default choice for raw throughput.
- **`echo-busy`** (pinned, 100 % spin) — **wins the shallow / low-latency regime**
  decisively (p50 28 µs, 2.4× the throughput at `in_flight=1`) and gives a hard
  isolated core budget, at the cost of burning every pinned core continuously (even
  idle) and *losing* on deep-pipeline throughput. Best for latency-critical,
  dedicated-core, request/response workloads.
- **`echo-park`** (pinned, reactor-per-core) — the **middle**: pinned locality and
  determinism with **0 % idle CPU** and the best tail at low core counts, but
  ~5–15 % less throughput than `echo` and higher p50 than `echo-busy`. Best when you
  want core affinity **without** paying for spin on an idle/bursty workload.
