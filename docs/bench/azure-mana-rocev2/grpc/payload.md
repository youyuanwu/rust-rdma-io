# gRPC payload size (64 B → 8 KiB)

Throughput and latency vs payload size, and the peak 8 KiB large-payload ceiling
per transport (the inverse of the raw-echo result). See the
[gRPC scenario](../../scenarios/grpc.md), [methodology](../../methodology.md), and
[metrics](../../metrics.md). Other gRPC regimes:
[throughput & pipelining (64 B)](throughput-64b.md) ·
[client CPU & memory](cpu-memory.md).

## Results

### 2026-07-17 — regression re-validation
- **Date:** 2026-07-17
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- `duration=10 warmup=3`, rings at 8 KiB use `ring_max_msg=9216`, reboot-clean NIC

Re-run on the current binary as a regression check. **No regression** — and because
the large-payload sweeps hold `in_flight=1` (the deadlock-safe regime) every point
was measurable and matches baseline.

**Payload size (8 conns / 8 threads)** — throughput, vs baseline:

| payload | in-flight | send-recv | read-ring | credit-ring | tcp |
|---:|---:|---:|---:|---:|---:|
| 64 B  | 1 | 51.7K | 50.9K | 56.8K | 50.7K |
| 256 B | 8 | 82.2K (105.0K) | 110.1K (115.6K) | 85.7K (115.2K) | 102.3K (101.5K) |
| 8 KB  | 1 | 29.9K (29.8K) | 47.2K (44.9K) | 47.3K (47.1K) | 41.2K (31.8K) |

(read-ring and tcp track baseline at every size; the credit-ring/send-recv 256 B
points came in ~25 % soft this single run — run-to-run gRPC variance, as both are
healthy at 64 B depth and 8 KB here.)

**Large-payload (8 KiB) peak per transport** — held `in_flight=1`, tuned
connections:

| transport | best config | throughput | bandwidth | vs baseline |
|---|---|---:|---:|---|
| **tcp** | 256 × 8 | **431K** | 28.3 Gbps | 427K / 28.0 |
| **read-ring** | 128 × 1 | 245K | 16.1 Gbps | 247K / 16.2 |
| **credit-ring** | 48 × 1 | 204K | 13.4 Gbps | 202K / 13.2 |
| **send-recv** | 192 × 1 | 114K | 7.5 Gbps | 110K / 7.2 |

Connection scaling behind each peak reproduced cleanly (read-ring 153/208/228/245K
at 32/64/96/128×1; credit-ring 91/159/204K at 16/32/48×1; send-recv 97/109/114K at
64/128/192×1; tcp 277/348/417/428K at 32/64/128/256×16). TCP still wins the 8 KiB
gRPC headline; the one-sided rings win latency at their peak — unchanged.

### Undated — historical baseline
- **Date:** unknown (baseline result set predates dated recording)
- **Environment:** [azure-mana-rocev2](../README.md)
- **Commit:** `unknown` (not recorded)
- **Command:** `TODO — not recorded`
- 8 conns / 8 threads unless noted; large-payload sweeps `threads=64`, reboot-clean NIC

#### Payload size (8 conns / 8 threads)

Throughput (p99 latency µs):

| Payload | in-flight | send-recv | read-ring | credit-ring | tcp |
|---:|---:|---:|---:|---:|---:|
| 64 B  | 1 | 53.5K (228) | 54.9K (226) | **58.5K** (216) | 48.2K (249) |
| 256 B | 8 | 105.0K (969) | **115.6K** (991) | 115.2K (995) | 101.5K (1169) |
| 8 KB  | 1 | 29.8K (389) | 44.9K (278) | **47.1K** (259) | 31.8K (348) |

At an 8 KB payload the one-sided ring transports (44.9K / 47.1K) outrun both
`send-recv` (29.8K) and TCP (31.8K), at roughly half the p99 latency (259–278 µs vs
348–389 µs).

#### Large-payload (8 KiB) ceiling — max throughput per transport

The payload-size table above is `in_flight=1` at a fixed 8 conns. Pushing each
transport for **peak** 8 KiB throughput (tuning connections *and* in-flight,
`threads=64`, reboot-clean NIC) tells the opposite story to the raw-transport
[echo 8 KiB result](../echo/large-payload-8kib.md#large-payload-8-kib-ceiling-read-ring-vs-tcp),
where read-ring *wins*. Best config = `connections × in-flight`:

| transport | best config | **max throughput** | bandwidth | p50 | p99 | CPU/op | ~cores | limited by |
|---|---|---:|---:|---:|---:|---:|---:|---|
| **tcp** (kernel) | 256 × 8 | **427K** | 28.0 Gbps | 4411 µs | 10167 µs | 118 µs | ~50 | bandwidth + CPU wall |
| **read-ring** | 128 × 1 | **247K** | 16.2 Gbps | 486 µs | 1090 µs | 95 µs | ~24 | deadlock @ ~160 conns |
| **credit-ring** | 48 × 1 | **202K** | 13.2 Gbps | 227 µs | 422 µs | 94 µs | ~19 | deadlock @ ~64 conns |
| **send-recv** | 192 × 1 | **110K** | 7.2 Gbps | 1745 µs | 2623 µs | 117 µs | ~13 | 2-sided recv round-trip |

Scaling behind each peak (RDMA held at `in_flight=1` — connections drive throughput;
deeper pipelines make the RDMA transports *worse*, e.g. `send-recv` at `in_flight=16`
collapsed to a **1.3 s p99**):

- **tcp**: 32→64→128→256 conns × 16 = 273K→343K→412K→426K; 256 × 8 = **427K**
  (384 conns regresses). ~50 cores near the ~28 Gbps wall.
- **read-ring**: 32→64→96→128 × 1 = 153K→202K→229K→**247K**; **160 × 1 deadlocks**
  (warmup hang — onset lower than 64 B's ~208 because 8 KiB fills the byte ring
  faster).
- **credit-ring**: 16→32→48 × 1 = 91K→156K→**202K**; **64 × 1 deadlocks**.
- **send-recv**: 64→128→192 × 1 = 99K→107K→**110K** (plateaus, no deadlock).

**At the gRPC layer TCP wins the 8 KiB headline (427K) — the inverse of `echo`,
where read-ring leads at 36 Gbps.** Two things flip it: (1) gRPC 8 KiB is
stack-bound (~95–118 µs/op, TLS + h2 + protobuf), and (2) **every RDMA transport
hits the high-fan-out flow-control deadlock** — read-ring at ~160 connections,
credit-ring at ~64 — capping them at **13–24 of 64 cores**, far below TCP's ~50-core
/ 28 Gbps wall. TCP has no RDMA recv-pool/ring to exhaust, so it scales to 256
connections. `in_flight>1` only makes the RDMA transports worse.

But **RDMA still wins latency by ~10×** at its own peak (read-ring p50 486 µs / p99
1090 µs vs TCP p50 4411 / p99 10167), because it peaks at far lower concurrency —
and it leaves ~30–40 cores idle. read-ring was **still climbing at 128 × 1**
(16.2 Gbps, ~24 cores): if the high-fan-out deadlock
([read-ring-concurrent-stream-deadlock.md](../../../bugs/read-ring-concurrent-stream-deadlock.md))
were lifted it could chase the same ~28 Gbps bandwidth wall TCP hits. The one-sided
rings (read-ring, credit-ring) beat `send-recv` here, which is throttled by its
per-message two-sided recv round-trip.

> Carrying a whole 8 KiB gRPC message in one RDMA message needs
> `--ring-max-msg ≥ 8215` (payload + protobuf/gRPC/HTTP-2 DATA framing ≈ +17–23 B;
> HEADERS frames are separate writes). These runs used `--ring-max-msg 9216`
> (9 KiB) — above the DATA frame, and still `65536/9216 ≈ 7` ring slots.
> Fragmentation is *not* what triggers the deadlock (the same 8 KiB payload runs
> cleanly at `in_flight=1`, low fan-out); connection count is.
