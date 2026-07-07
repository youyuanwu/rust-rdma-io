# Direct Transport Echo Benchmark (`--mode echo`)

**Status**: ✅ Implemented — [`tests/rdma-io-bench/src/echo.rs`](../../tests/rdma-io-bench/src/echo.rs)
**Purpose**: Measure raw transport request/response performance without the tonic/gRPC stack.

## 1. Motivation

The `rh2` and `rh3` benchmark modes drive gRPC over RDMA, stacking four layers on
top of the transport:

- **TLS 1.3** (encrypt/decrypt + record framing)
- **HTTP/2** (or QUIC for `rh3`) — `h2`'s `FramedWrite` coalesces small frames
  into one buffer (`CHAIN_THRESHOLD`), so the transport never sees the write
  pattern the application produced
- **protobuf** serialization
- **hyper** reactor wakeups + stream bookkeeping

Those layers make transport-level tuning hard to observe: vectored writes and a
native `tokio` I/O path both measured *flat* through `rh2` precisely because h2
pre-coalesces (see [TonicRdmaVsTcpPerformance.md](TonicRdmaVsTcpPerformance.md)).

`--mode echo` removes all four. Each request is **one `send_copy`**, each
response is **one recv completion**, so latency and throughput map 1:1 to
transport behaviour. A matched raw-TCP echo (`--transport tcp`) provides an
apples-to-apples kernel baseline, and `--in-flight N` controls how many requests
each connection keeps outstanding so pipelining depth can be swept directly.

## 2. Where it sits in the stack

`echo` is generic over the [`Transport`](../../rdma-io/src/transport.rs) trait, so
one implementation covers all three RDMA transports plus a raw-TCP baseline.

```mermaid
graph TB
    subgraph rh2["rh2 / rh3 (tonic path)"]
        direction TB
        A1["Greeter gRPC client"]
        A2["protobuf"]
        A3["HTTP/2 (h2) / HTTP/3 (quinn)"]
        A4["TLS 1.3"]
        A5["AsyncRdmaStream / RdmaUdpSocket"]
        A1 --> A2 --> A3 --> A4 --> A5
    end

    subgraph echo["--mode echo (this doc)"]
        direction TB
        B1["echo client loop<br/>(send_copy / poll_recv)"]
        B1 --> B2
    end

    A5 --> T["Transport trait<br/>send-recv · read-ring · credit-ring"]
    B2["Transport trait / TcpStream"] --> T
    T --> NIC["RDMA NIC (RoCEv2) · or kernel TCP"]

    style echo fill:#e8f5e9
    style rh2 fill:#fff3e0
```

The tonic path traverses five layers before reaching the transport; the echo
path calls the transport directly.

## 3. RDMA raw-transport echo

### 3.1 Request/response pipeline

Each connection runs a single task that owns the transport (the `Transport`
trait is `&mut self` / not split), keeping up to `--in-flight N` requests
outstanding. RC queue pairs preserve ordering and the echo server replies in
receive order, so responses match requests **FIFO** via a `VecDeque<Instant>` of
send timestamps.

```mermaid
sequenceDiagram
    participant C as Echo client<br/>(one connection)
    participant Q as send_times<br/>(VecDeque)
    participant S as Echo server

    Note over C: fill pipeline up to N
    C->>Q: push t0
    C->>S: send_copy(req #1)
    C->>Q: push t1
    C->>S: send_copy(req #2)
    C->>Q: push t2
    C->>S: send_copy(req #3)
    Note over C: N in flight → await completions

    S-->>C: echo #1 (recv completion)
    C->>Q: pop t0 → record RTT, in_flight--
    C->>Q: push t3
    C->>S: send_copy(req #4)

    S-->>C: echo #2
    C->>Q: pop t1 → record RTT, in_flight--
    C->>Q: push t4
    C->>S: send_copy(req #5)
```

### 3.2 Client poll loop

The task alternates between **filling** the pipeline (synchronous `send_copy`
until the depth target is hit or the transport returns `Ok(0)`) and **awaiting
progress** via a `poll_fn` bounded by the benchmark deadline. A single poll
checks three sources in priority order:

```mermaid
flowchart TD
    Start([outer loop]) --> Deadline{now ≥ bench<br/>deadline?}
    Deadline -- yes --> Done([disconnect + return<br/>histogram, errors])
    Deadline -- no --> Fill

    Fill["fill: while in_flight < target<br/>send_copy(payload)"] --> FillR{result}
    FillR -- "Ok(0)" --> Poll
    FillR -- "Ok(n)" --> FillPush["push send time<br/>in_flight++"] --> Fill
    FillR -- Err --> Done

    Poll["timeout_at(deadline, poll_fn)"] --> PS{poll_send_<br/>completion}
    PS -- "Ready(Ok)" --> Progress([Progress → loop])
    PS -- "Ready(Err)" --> Closed([Closed → break])
    PS -- Pending --> PR{poll_recv}

    PR -- "Ready(Ok n)" --> Rec["for each completion:<br/>pop send time → record RTT<br/>in_flight--; repost_recv"] --> Progress
    PR -- "Ready(Err)" --> Closed
    PR -- Pending --> PD{poll_disconnect}
    PD -- true --> Closed
    PD -- false --> Pend([Pending → await wake])
```

Reaping a send completion first frees a send buffer so the next `fill` can post
more. `AsyncCq::poll_completions` only returns `Ready(Ok(n))` with `n ≥ 1`, so a
send-completion wake is always real progress (no busy-loop). Latency is recorded
only when the request's send time is at or after the warmup deadline.

### 3.3 Server echo loop

The server accepts connections until shutdown and spawns one echo loop per
connection. Because `recv_buf(idx)` borrows the transport immutably while
`send_copy` needs `&mut`, each message is **copied into an owned buffer** (from a
pooled free-list) so the recv buffer can be reposted immediately; the copy is
then queued and echoed back with back-pressure handling.

```mermaid
flowchart TD
    Loop([per-connection loop]) --> Flush

    Flush["flush: while pending not empty<br/>send_copy(front)"] --> FR{result}
    FR -- "Ok(0)" --> Poll
    FR -- "Ok(n)" --> Recycle["pop front → clear → pool"] --> Flush
    FR -- Err --> End([disconnect + return])

    Poll["poll_fn"] --> PS{poll_send_completion}
    PS -- "Ready(Ok)" --> Alive([alive → loop])
    PS -- "Ready(Err)" --> Dead([dead → break])
    PS -- Pending --> PR{poll_recv}

    PR -- "Ready(Ok n)" --> Copy["for each completion:<br/>copy recv_buf[..len] → pooled Vec<br/>repost_recv; push to pending"] --> Alive
    PR -- "Ready(Err)" --> Dead
    PR -- Pending --> PD{poll_disconnect}
    PD -- true --> Dead
    PD -- false --> Pend([Pending → await wake])
```

### 3.4 Buffer sizing

The raw path lets the benchmark size the transport to the workload — something
the tonic path's fixed `::stream()`/`::datagram()` configs don't expose.

| Side | send-recv config | Notes |
|---|---|---|
| Client | `buf_size = payload.max(64)`, `num_send_bufs = num_recv_bufs = in_flight + 2` | Sized so `N` requests fit in flight |
| Server | `buf_size = 64 KiB`, `num_send_bufs = num_recv_bufs = 64` | Generous fixed sizing; handles pipelined clients |
| Rings | `::datagram()` (`max_message_size = 1500`) + `--mw-fallback` | Payloads > 1500 B are truncated to one message (warned) — do **not** raise `max_message_size` (latent ring bug, see perf doc) |

## 4. Raw-TCP baseline (`--transport tcp`)

TCP is a byte stream, so pipelining needs the connection split into independent
reader/writer halves coordinated by a semaphore. The server echoes bytes
verbatim; since every request is exactly `payload` bytes, each `read_exact` of
`payload` bytes is one response.

```mermaid
flowchart LR
    subgraph Client["TCP client (one connection)"]
        direction TB
        SEM["Semaphore(N permits)"]
        W["writer task:<br/>acquire permit → push send time<br/>write_all(payload)"]
        R["reader task:<br/>read_exact(payload)<br/>pop send time → record RTT<br/>add_permits(1)"]
        SEM -. permit .-> W
        R -. return permit .-> SEM
    end

    W ==>|payload bytes| SRV["TCP echo server<br/>read → write_all"]
    SRV ==>|echoed bytes| R
```

The semaphore bounds outstanding requests to `N`; the reader returns a permit
per response, and closing the semaphore stops the writer at the deadline.

## 5. Metrics and reporting

Both paths reuse [`BenchMetrics`](../../tests/rdma-io-bench/src/metrics.rs) /
[`BenchResult`](../../tests/rdma-io-bench/src/report.rs). Each connection records
into its own `hdrhistogram`; the per-connection histograms are merged at the end
into throughput + p50/p95/p99/p999/min/max/avg, reported with `mode = "echo"`
and `transport = send-recv | read-ring | credit-ring | tcp`.

For `--mode echo`, the client additionally samples its own CPU and memory over
the measured window (see §8) and reports `cpu_seconds`, `cpu_us_per_op`, and
`peak_rss_kb` alongside the latency stats.

## 6. Running

```bash
# RDMA send-recv echo, 8 connections, 16 requests in flight each, 1 KiB payload
rdma-bench-server --mode echo --transport send-recv --bind 0.0.0.0:50051
rdma-bench-client --mode echo --transport send-recv \
    --connect <server-ip>:50051 --connections 8 --in-flight 16 --payload 1024

# TCP baseline (same knobs)
rdma-bench-server --mode echo --transport tcp --bind 0.0.0.0:50051
rdma-bench-client --mode echo --transport tcp \
    --connect <server-ip>:50051 --connections 8 --in-flight 16 --payload 1024
```

Transports: `send-recv`, `read-ring`, `credit-ring` (raw RDMA) and `tcp`
(baseline). Add `--mw-fallback` for the ring transports on NICs that report
`max_mw = 0` (e.g. Azure MANA).

**Ring queue sizing.** By default a ring sizes its send/doorbell/CQ queues for
`ring_capacity / max_message_size` in-flight messages (~43 at the 1500 B
default), which caps deep pipelines of small messages. Two knobs raise it:

- `--ring-queue-depth N` — size the queues for `N` in-flight messages directly
  (preferred; keeps the message size). Must match on client and server.
- `--ring-max-msg B` — lower the max message size (also raises the slot count),
  at the cost of truncating payloads larger than `B`.

```bash
# read-ring, 64 in flight per connection, queues sized for 128
rdma-bench-server --mode echo --transport read-ring --mw-fallback --ring-queue-depth 128 --bind 0.0.0.0:50051
rdma-bench-client --mode echo --transport read-ring --mw-fallback --ring-queue-depth 128 \
    --connect <server-ip>:50051 --connections 8 --in-flight 64 --payload 64
```

### Sweeping the matrix on the VMs

The outer repo drives the two-VM sweep over Ansible (mirrors `bench-matrix`):

```bash
just deploy-bench                                   # build + push binaries/certs
just echo-matrix                                    # transports x in-flight, 8x8
just echo-matrix "send-recv tcp" "1 8 32" 8 10 1024 # subset + 1 KiB payload
# full sweep with ring queues sized for deep pipelines (positional args:
# transports in_flights cpus duration payload mw_fallback settle reboot_between ring_queue_depth)
just echo-matrix "send-recv read-ring credit-ring tcp" "1 4 16 64" 8 8 64 true 10 false 128
just bench-report                                   # merge JSON → Markdown + charts
```

`echo-matrix` iterates `transports x in_flights`, invoking the shared
`bench_run.yml` playbook with `bench_mode=echo` and `bench_in_flight=N`. The
trailing `ring_queue_depth` arg (default `0` = derive from `ring_capacity /
max_message_size`) sizes the ring queues so the ring transports sustain the
deeper in-flight values without back-pressuring early (see §7). Results land in
`build/bench/bench-echo-<transport>-<conns>conn-<threads>thr-<N>if.json`;
`in_flight` is carried in the result JSON, so `bench-report` keeps each
`echo/<transport>@if<N>` point distinct. `settle`/`reboot_between` behave as in
`bench-matrix` (idle pause vs NIC power-cycle between combos).

### Debugging a failed run

Pass `save_logs=true` (optionally with `rust_log=...`) to keep the server and
client `RUST_LOG` output on the controller for a single run:

```bash
just run-bench mode=echo transport=read-ring in_flight=64 payload=64 \
    mw_fallback=true rust_log="rdma_io_bench=trace,rdma_io=debug" save_logs=true
# → build/bench/client-echo-read-ring-64if.log, build/bench/server-read-ring-64if.log
```

## 7. Findings and measured results

The measured findings — the ring in-flight ceiling, CPU-efficiency and depth
scaling, the read-ring / TCP throughput ceilings, and the 2026-07-07
re-validation — have moved to [../bench/echo.md](../bench/echo.md).
