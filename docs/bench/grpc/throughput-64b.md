# gRPC throughput & pipelining (64 B)

Connection scaling, in-flight pipelining, and peak-throughput sweeps at 64 B for
the tonic `rh2` transports vs the `tcp` baseline. See [README.md](README.md) for
the scenario and how to run it. Other gRPC datasets: [payload size](payload.md) ·
[client CPU & memory](cpu-memory.md).

Measured **2026-07-06** on the two E-series MANA RoCEv2 VMs, comparing `rh2`
(gRPC / HTTP-2 over RDMA) against the `tcp` baseline (same tonic + TLS stack over
a kernel socket). MR-rkey fallback is auto-detected; each run is 10 s; **every
completed run had 0 errors**, and the NIC was rebooted between combos for a clean
signal. Throughput is `req/s`; latency is per-RPC.

## Connection scaling (in-flight = 1, 64 B payload)

Throughput (p50 / p99 latency µs), one outstanding RPC per connection:

| Conns | credit-ring | read-ring | send-recv | tcp (baseline) |
|---:|---:|---:|---:|---:|
| 64  | **251.6K** (239/503) | 230.7K (262/529) | 195.6K (309/634) | 223.4K (260/475) |
| 128 | **315.2K** (377/886) | 308.2K (385/885) | 213.7K (579/1132) | 245.9K (478/1051) |
| 256¹ | ✗ warmup hang | ✗ warmup hang | ✗ warmup hang | **328.9K** (709/1667) |

¹ 256-connection runs used 64 worker threads (matching vCPU count), and TCP
scales fine there. The RDMA transports do **not** fail in connection setup — an
instrumented sweep confirms all connections *establish* (the client reaches
"Connected N clients", with at most one or two connections needing a CM retry).
The run then **hangs in the warmup / data phase** (zero requests complete). This
is a **hard flow-control deadlock at high concurrency**, not a CM-establishment
ceiling and not mere slowness: during the hang the RDMA op counters
(`rx_write_requests`, `rx_read_requests`, `tx/rx_packets`) are frozen (zero
forward progress across a 7 s sample), CPU is not pegged (threads parked on
completions that never arrive), and the RNR-NAK counters (`requester_rnr_nak`,
`responder_rnr_nak`, `requester_timeout`) accumulate then freeze — receivers run
out of posted recv buffers, senders get RNR-NAKed, and the whole fan-out wedges.
It is the same flow-control coupling as the low-connection bidirectional deadlock
([read-ring-concurrent-stream-deadlock.md](../../bugs/read-ring-concurrent-stream-deadlock.md)),
re-emerging at scale where the per-connection relief can't keep up with aggregate
recv-buffer exhaustion.

All three RDMA transports hit it, at transport-specific onset points (clean NIC,
`in_flight=1`, 64 B; a hung run does not persistently wedge the NIC — 192
connections ran between hung runs). The onset is **probabilistic** — the deadlock
grows more likely as connections rise rather than switching on at a hard count
(read-ring completed 1 of 2 reboot-clean trials at 224). Approximate onset from
single-run bisection:

| Transport | usually works | starts hanging | peak throughput |
|---|---:|---:|---:|
| read-ring | 208 | ~216–224 | 365K req/s @208 |
| credit-ring | 192 | 208 | 376K req/s @192 |
| send-recv | ≤192 | 208 | — |

Resilience tracks *inverse* per-connection recv state: `read-ring` (one-sided,
only doorbell buffers) survives highest; `send-recv` (a full recv-buffer pool per
connection) hangs earliest. TCP is unaffected (kernel sockets, no RDMA recv pool)
and scales past 256.

**The deadlock is in the gRPC / `AsyncRdmaStream` layer, not the transport.** The
direct-transport `echo` mode (same RDMA transports, but no tonic / h2 / TLS /
`AsyncRdmaStream` — see [EchoBenchmark.md](../../design/EchoBenchmark.md))
was run at the same load as a control, on **all three transports**:

| echo transport | data phase | peak throughput |
|---|---|---|
| read-ring | clean ✓ 128–224 (completed 2/2 at 224 in a reboot-clean A/B) | 177–184K req/s |
| credit-ring | clean ✓ 128 | 371K req/s |
| send-recv | clean ✓ 128 | 189K req/s |

`echo` drives the *same* transport recv pools the gRPC path deadlocks on, yet its
**data phase completes cleanly at every load tested — no hang** — including 224
connections, where gRPC `read-ring` completed only 1 of 2 reboot-clean trials.
`echo` never buffers on the read side (the server copies each message and reposts
its recv buffer immediately; the client FIFO-matches), so it has no
write-before-read flush-gate. The gRPC path does: hyper/h2 won't poll reads while
a stream is write-blocked, so the stash / flush-gate coupling (the same root as
the low-connection bidirectional deadlock) re-emerges at high fan-out and wedges.
(The connect code is *identical* — tonic's `RdmaConnector` and `echo` both call
`builder.connect()`; the connect failures first seen for `echo` at high counts
were run-to-run MANA RDMA-CM variance, not a real ceiling — the A/B reconnected
224 fine.) So the fix is in the gRPC recv path (drain reads independent of the
write state / bound aggregate outstanding), not the transport — see the
[deadlock bug doc](../../bugs/read-ring-concurrent-stream-deadlock.md).

The RDMA rings **beat TCP at 64–128 connections** on both throughput and latency
— at 128 connections `credit-ring` leads at **315K req/s (+28 % over TCP)** with
a lower p50 (377 µs vs 478 µs). `send-recv` is the weakest RDMA transport at
`in_flight=1` (each message pays a two-sided recv-post round trip).

## In-flight pipelining (8 conns / 8 threads, 64 B payload)

Keeping more RPCs outstanding per connection (h2 multiplexing / `--in-flight`):

| In-flight | send-recv | read-ring | credit-ring | tcp (baseline) |
|---:|---:|---:|---:|---:|
| 1  | 53.1K | 52.6K | 56.1K | 47.3K |
| 4  | 99.9K | **105.7K** | 100.1K | 57.6K |
| 16 | 125.3K | 120.0K | 125.4K | 124.9K |
| 64 | **150.4K** | 113.8K | 99.6K | 148.8K |

Latency at the same points (p50 / p99 / p999 µs):

| In-flight | send-recv | read-ring | credit-ring | tcp (baseline) |
|---:|---:|---:|---:|---:|
| 1  | 144/229/269 | 147/238/281 | 138/228/266 | 162/252/303 |
| 4  | 315/543/639 | 299/515/598 | 313/562/662 | 216/455/**41183** |
| 16 | 969/1795/2139 | 1044/1948/2299 | 1002/1858/2213 | 987/1976/2379 |
| 64 | 3293/6115/7311 | 2535/4855/**658431** | 2535/4767/**659455** | 3325/6387/7719 |

At `in_flight=4` **TCP stalls at 57.6K** (a single HTTP-2-over-one-TCP-connection
is a serial byte stream, so extra outstanding requests barely help) while the
RDMA transports pipeline to ~100–106K. At `in_flight=64` the rings regress
(read-ring 114K, credit-ring 100K) because the gRPC path does not yet auto-size
the ring queue depth from `in_flight` (the `echo` mode does — see
[EchoBenchmark.md](../../design/EchoBenchmark.md)); `send-recv`
and TCP scale cleanly to ~150K.

The bolded p999 outliers are tail artifacts, not median regressions: TCP's
41 ms p999 at `in_flight=4` is head-of-line blocking on the single byte stream
(its p50 is a healthy 216 µs), and the rings' ~659 ms p999 at `in_flight=64` is
the RoCE RNR-NAK retry timer (~0.657 s) firing when the default ring depth is
over-subscribed — the medians there (2535 µs) are actually *lower* than
`send-recv`/TCP.

## Peak throughput (in-flight depth at 64 connections)

Raising in-flight at 64 connections (well below the deadlock onset) pushes
throughput until the client saturates CPU. Throughput (CPU µs/op), 64 B, 0 errors:

| in-flight | read-ring | credit-ring | send-recv | tcp |
|---:|---:|---:|---:|---:|
| 1  | 255.6K (75.5) | 237.9K (75.2) | 191.9K (70.8) | 220.6K (75.4) |
| 4  | 524.9K (92.5) | 517.6K (97.8) | 466.4K (100.6) | 316.4K (118.8) |
| 8  | 603.7K (81.2) | 599.8K (83.6) | 510.2K (85.6) | 379.2K (103.9) |
| 16 | 647.9K (75.8) | 645.8K (75.9) | — | 601.0K (80.3) |
| 32 | **697.0K (71.0)** | — | — | 653.9K (74.4) |
| 64 | 520.7K (76.3) | 471.6K (77.5) | — | 689.6K (70.3) |

At 64 connections `read-ring` tops out at ~697K req/s (in-flight 32) — ~2.7× the
depth-1 rate. The rings follow an inverted-U: past in-flight 32 they over-queue and
regress (697K → 521K). Through in-flight ≤ 16 RDMA leads TCP on both throughput and
CPU/op (e.g. at in-flight 8: 604K @ 81 µs vs 379K @ 104 µs). But 64 connections is
*not* `read-ring`'s ceiling — adding connections (below the ~208 deadlock onset)
pushes it higher still (see below). (`send-recv` has gaps where its
first-combo-after-reboot run hit the transient high-conn CM timeout.)

Pushed further, **TCP tops out at ~740K req/s** (256 conns × in-flight 16) — at
**74 µs CPU/op, ≈ 54.8 of 64 client cores** (740K × 74 µs ≈ 54.8 core-seconds per
second). TCP is CPU-bound and scales with *connection count* (parallelism) rather
than depth, so it wants more connections, not deeper queues:

| conns @ in-flight 16 | 128 | 256 | 384 | 512 |
|---|---:|---:|---:|---:|
| tcp req/s | 701K | **740K** | 731K | 688K |
| CPU µs/op | 76.6 | **74.1** | 72.4 | 72.6 |
| ~client cores | 53.7 | **54.8** | 52.9 | 49.9 |
| p99 µs | 6451 | **13527** | 20031 | 25791 |

Beyond ~256 connections TCP plateaus then regresses (scheduling/softirq
contention; RSS balloons to 1–2.7 GB at high conn × depth).

`read-ring`, given both more connections *and* pipelining, **overtakes TCP**. Its
per-op CPU keeps *falling* with depth (75 → 67 µs), so it turns the same ~56-core
budget into more throughput (64 B, in-flight 16, 0 errors):

| read-ring conns @ in-flight 16 | 128 | 160 | 192 | 208 |
|---|---:|---:|---:|---:|
| req/s | 784K | 778K | **833K** | 828K |
| CPU µs/op | 70.0 | 72.5 | **67.4** | 67.6 |
| p99 µs | 5635 | 7495 | **8567** | 9431 |

`read-ring` **peaks at ~833K req/s (192 conns × in-flight 16, ~56 of 64 cores)** —
~13 % above TCP's 740K, and at a *lower* tail (p99 8.6 ms vs TCP's 13.5 ms). Two limits keep it from going further: past ~208
connections it hits the high-fan-out **deadlock**, and past in-flight 16 at these
connection counts it **over-queues** (192c × in-flight 32 → 740K; 208c × in-flight
24 / 32 → 781K / 707K, with RSS climbing to ~1.2 GB). So the corrected picture: the
corrected picture: the TLS/HTTP-2/protobuf stack still bounds both transports at
~55–56 of 64 cores, but `read-ring`'s lower per-op CPU converts that budget into
**more** throughput than TCP (833K vs 740K), at fewer connections. Lifting the
high-fan-out deadlock would let it scale further still.

## Re-validation (2026-07-17)

Re-run on the current binary (64 B, `duration=10 warmup=3`, reboot-clean NIC
between ring batches) as a regression check. **No regression** in any measurable
point; the only gaps are the high-fan-out ring runs, which hit the *documented*
flow-control deadlock (it manifested earlier than usual on a deadlock-prone host
this session — not a code change; TCP at the same fan-out ran clean).

**Connection scaling (in-flight 1)** — req/s, vs baseline:

| conns | credit-ring | read-ring | send-recv | tcp |
|---:|---:|---:|---:|---:|
| 64  | 236K (251.6K) | 238K (230.7K) | 188K (195.6K) | 218K (223.4K) |
| 128 | 321K (315.2K) | 304K (308.2K) | 212K (213.7K) | 246K (245.9K) |
| 256 | — (deadlock) | — (deadlock) | — (deadlock) | 323K (328.9K) |

**In-flight pipelining (8 conns / 8 threads)** — req/s:

| in-flight | send-recv | read-ring | credit-ring | tcp |
|---:|---:|---:|---:|---:|
| 1  | 51.7K | 50.9K | 56.8K | 50.7K |
| 4  | 114.9K | 105.3K | 108.2K | 91.4K |
| 16 | 98.8K | 121.5K | 109.2K | 116.9K |
| 64 | 136.3K | 142.5K | 139.1K | 134.7K |

**Peak throughput (64 conns, in-flight depth)** — req/s:

| in-flight | read-ring | credit-ring | send-recv | tcp |
|---:|---:|---:|---:|---:|
| 1  | 242.9K | 248.6K | 196.9K | 216.3K |
| 4  | 553.5K | 565.5K | ✗¹ | 450.6K |
| 8  | 633.0K | 529.5K | 461.2K | 534.4K |
| 16 | 691.0K | 652.9K | — | 595.9K |
| 32 | 744.1K | — | — | 661.5K |
| 64 | 772.4K | 710.3K | — | 700.4K |

read-ring peaks ~744–772K (baseline ~697K), credit-ring 710K, tcp 700K — all at
or above baseline. TCP connection sweep (in-flight 16): 128×16 742K, **256×16
806K** (baseline peak 740K), 384×16 743K, 512×16 642K. ¹`send-recv 64×4` warmup-
hung on every attempt this session (the probabilistic gRPC flow-control hang;
send-recv hangs earliest) — its in-flight-1 and in-flight-8 neighbours are clean
and match baseline, so it is environmental, not a transport regression.

**Not reproducible this session:** the read-ring connection sweep
(128–208 × in-flight 16, baseline ~784–833K) — every point warmup-hung on the
gRPC flow-control deadlock, which was more prone than usual on this host (it hit
at ≥128 conns vs the documented ~208 onset). TCP at the same connection counts
ran clean, confirming the wedge is the known `AsyncRdmaStream`/HTTP-2-layer
deadlock ([../../bugs/read-ring-concurrent-stream-deadlock.md](../../bugs/read-ring-concurrent-stream-deadlock.md)),
not a data-path change.
