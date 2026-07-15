//! Direct transport-level echo benchmark (no tonic/gRPC/TLS/HTTP).
//!
//! The `rh2`/`rh3` benchmark paths stack TLS, HTTP/2 (or QUIC), protobuf, and
//! the hyper reactor on top of the RDMA transport. Those layers coalesce and
//! reframe writes (e.g. h2's `FramedWrite` chain threshold), which hides the
//! transport's true behaviour and makes transport-level tuning hard to measure.
//!
//! This module implements a minimal request/response *echo* directly on the
//! [`Transport`](rdma_io::transport::Transport) trait — one `send_copy` per
//! request, one recv completion per response — plus a matched raw-TCP echo
//! baseline. Both reuse [`BenchMetrics`]/[`BenchResult`] so results line up with
//! the tonic modes. An `--in-flight N` knob controls how many requests each
//! connection keeps outstanding, so the effect of pipelining depth on the raw
//! transport can be measured directly.
//!
//! See `docs/design/EchoBenchmark.md` for a full description and diagrams of the
//! client pipeline, the server echo loop, and the TCP baseline.
//!
//! ```text
//! client (one task/connection)                 server (one task/connection)
//!   fill: send_copy(req) ×N  ───────────────▶  poll_recv → copy → pending
//!   await: poll_send_completion (free buf)      flush: send_copy(echo)
//!          poll_recv → pop send_time → RTT  ◀───────────────
//!          repost_recv; keep pipeline full
//! ```

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use rdma_io::async_cm::{AsyncCmId, AsyncCmListener};
use rdma_io::cm::PortSpace;
use rdma_io::device::Context;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::transport::{RecvCompletion, Transport, TransportBuilder};
use rdma_io_busy::{ArmParkPool, BusyPool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};

use crate::metrics::spawn_resource_sampler;
use crate::report::BenchResult;

/// Recv completions to drain per poll (the send/recv transports cap at 8).
const RECV_BATCH: usize = 8;

/// Per-attempt connect timeout. On the MANA RoCEv2 preview a CM handshake can
/// hang (not just fast-fail) under connect churn, so each attempt is bounded and
/// a hung handshake is retried rather than blocking the whole run.
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("create histogram")
}

/// Log a failed connect attempt and back off, returning `true` if the caller
/// should retry. Transient CM setup failures (e.g. the MANA RoCEv2 preview's
/// intermittent "Unreachable"/"Rejected" or a hung handshake at high connection
/// counts) usually succeed on a later attempt, so a small bounded retry avoids
/// aborting the whole run on one flaky connection. Returns `false` once
/// `retries` is exhausted, signalling the caller to propagate the error. The
/// backoff grows linearly (200ms × attempt).
async fn backoff_or_giveup(
    what: &str,
    tries: u32,
    retries: u32,
    err: &dyn std::fmt::Display,
) -> bool {
    if tries >= retries {
        return false;
    }
    let backoff = Duration::from_millis(200 * (tries as u64 + 1));
    eprintln!(
        "{what} connect attempt {} failed ({err}); retrying in {}ms...",
        tries + 1,
        backoff.as_millis()
    );
    tokio::time::sleep(backoff).await;
    true
}

/// Print an assembled [`BenchResult`] in the requested format.
#[allow(clippy::too_many_arguments)]
fn report_result(
    hist: &Histogram<u64>,
    mode: &str,
    transport: &str,
    connections: usize,
    threads: usize,
    in_flight: usize,
    duration: u64,
    payload: usize,
    errors: u64,
    cpu_seconds: f64,
    peak_rss_kb: u64,
    report: &str,
) {
    let result = BenchResult::from_histogram(
        hist,
        mode,
        transport,
        connections,
        threads,
        in_flight,
        duration,
        payload,
        errors,
    )
    .with_resource_usage(cpu_seconds, peak_rss_kb);
    match report {
        "json" => result.print_json(),
        _ => result.print_text(),
    }
}

// ---------------------------------------------------------------------------
// Raw RDMA transport echo
// ---------------------------------------------------------------------------

enum ClientStep {
    /// A send buffer was freed and/or responses were reaped — try to send more.
    Progress,
    /// The connection is dead (disconnect or fatal completion).
    Closed,
}

/// Client-side echo loop for one connection over a raw [`Transport`].
///
/// Keeps up to `target` requests outstanding, matching responses to requests in
/// FIFO order (RC queue pairs preserve ordering, and the echo server replies in
/// the order it receives). Returns the per-connection latency histogram and the
/// number of errors observed.
async fn client_conn_loop<T: Transport + 'static>(
    mut t: T,
    payload: Arc<[u8]>,
    target: usize,
    warmup_deadline: Instant,
    bench_deadline: Instant,
) -> (Histogram<u64>, u64) {
    let mut hist = new_histogram();
    let mut errors = 0u64;
    let mut send_times: VecDeque<Instant> = VecDeque::with_capacity(target + 2);
    let mut in_flight = 0usize;

    'outer: loop {
        if Instant::now() >= bench_deadline {
            break;
        }

        // Fill the pipeline up to the target depth.
        while in_flight < target {
            match t.send_copy(&payload[..]) {
                Ok(0) => break, // transport can't accept more right now
                Ok(_) => {
                    send_times.push_back(Instant::now());
                    in_flight += 1;
                }
                Err(e) => {
                    tracing::trace!(error = %e, "echo send_copy failed");
                    errors += 1;
                    break 'outer;
                }
            }
        }

        // Await progress, bounded by the benchmark deadline so the loop always
        // terminates even if the pipeline momentarily stalls.
        let deadline = tokio::time::Instant::from_std(bench_deadline);
        let step = tokio::time::timeout_at(
            deadline,
            poll_fn(|cx| {
                // Reap a send completion first to free a send buffer. The CQ
                // poll only reports Ready with n >= 1, so this is real progress.
                match t.poll_send_completion(cx) {
                    Poll::Ready(Ok(())) => return Poll::Ready(ClientStep::Progress),
                    Poll::Ready(Err(e)) => {
                        tracing::trace!(error = %e, "echo send cq error");
                        return Poll::Ready(ClientStep::Closed);
                    }
                    Poll::Pending => {}
                }

                // Reap responses.
                let mut out = [RecvCompletion::default(); RECV_BATCH];
                match t.poll_recv(cx, &mut out) {
                    Poll::Ready(Ok(n)) => {
                        let done = Instant::now();
                        for c in &out[..n] {
                            if let Some(start) = send_times.pop_front() {
                                if start >= warmup_deadline {
                                    let us =
                                        done.saturating_duration_since(start).as_micros() as u64;
                                    let _ = hist.record(us);
                                }
                                in_flight = in_flight.saturating_sub(1);
                            }
                            let _ = t.repost_recv(c.buf_idx);
                        }
                        Poll::Ready(ClientStep::Progress)
                    }
                    Poll::Ready(Err(e)) => {
                        tracing::trace!(error = %e, "echo recv cq error");
                        Poll::Ready(ClientStep::Closed)
                    }
                    Poll::Pending => {
                        if t.poll_disconnect(cx) {
                            Poll::Ready(ClientStep::Closed)
                        } else {
                            Poll::Pending
                        }
                    }
                }
            }),
        )
        .await;

        match step {
            Err(_elapsed) => break,
            Ok(ClientStep::Progress) => {}
            Ok(ClientStep::Closed) => {
                errors += 1;
                break;
            }
        }
    }

    let _ = t.disconnect();
    (hist, errors)
}

/// Run the raw-transport echo client: connect `connections` times, drive each
/// connection's pipelined echo loop, then report merged results.
#[allow(clippy::too_many_arguments)]
pub async fn run_transport_echo_client<B>(
    builder: B,
    addr: SocketAddr,
    connections: usize,
    connect_retries: u32,
    in_flight: usize,
    payload: usize,
    warmup: u64,
    duration: u64,
    threads: usize,
    transport_label: &str,
    report: &str,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let payload_buf: Arc<[u8]> = vec![b'x'; payload].into();

    let mut transports = Vec::with_capacity(connections);
    for _ in 0..connections {
        let mut tries = 0u32;
        let t = loop {
            match tokio::time::timeout(CONNECT_ATTEMPT_TIMEOUT, builder.connect(&addr)).await {
                Ok(Ok(t)) => break t,
                Ok(Err(e)) => {
                    if !backoff_or_giveup(transport_label, tries, connect_retries, &e).await {
                        return Err(e.into());
                    }
                }
                Err(_) => {
                    let e = format!("connect timed out after {CONNECT_ATTEMPT_TIMEOUT:?}");
                    if !backoff_or_giveup(transport_label, tries, connect_retries, &e).await {
                        return Err(e.into());
                    }
                }
            }
            tries += 1;
        };
        transports.push(t);
    }
    eprintln!(
        "Connected {connections} echo clients to {addr} \
         (mode=echo, transport={transport_label}, in_flight={in_flight}, threads={threads})"
    );

    let target = in_flight.max(1);
    let warmup_deadline = Instant::now() + Duration::from_secs(warmup);
    let bench_deadline = warmup_deadline + Duration::from_secs(duration);
    eprintln!("Warming up {warmup}s, then benchmarking {duration}s...");

    let sampler = spawn_resource_sampler(warmup_deadline, bench_deadline);

    let mut handles = Vec::with_capacity(connections);
    for t in transports {
        let p = payload_buf.clone();
        handles.push(tokio::spawn(client_conn_loop(
            t,
            p,
            target,
            warmup_deadline,
            bench_deadline,
        )));
    }

    let mut merged = new_histogram();
    let mut errors = 0u64;
    for (i, h) in handles.into_iter().enumerate() {
        match h.await {
            Ok((hist, errs)) => {
                errors += errs;
                merged.add(&hist).expect("merge histograms");
            }
            Err(e) => tracing::warn!(conn_id = i, error = %e, "echo client task panicked"),
        }
    }

    let (cpu_seconds, peak_rss) = sampler.await.unwrap_or((0.0, 0));

    report_result(
        &merged,
        "echo",
        transport_label,
        connections,
        threads,
        target,
        duration,
        payload,
        errors,
        cpu_seconds,
        peak_rss,
        report,
    );
    Ok(())
}

/// Server-side echo loop for one connection over a raw [`Transport`].
///
/// Copies each received message into an owned buffer (so the recv buffer can be
/// reposted immediately), then sends it back. A small free-list recycles the
/// owned buffers to avoid per-message allocation.
async fn server_conn_loop<T: Transport + 'static>(mut t: T) {
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pool: Vec<Vec<u8>> = Vec::new();

    loop {
        // Flush queued responses back to the peer.
        while let Some(front) = pending.front() {
            match t.send_copy(front.as_slice()) {
                Ok(0) => break, // send buffers full; drain completions below
                Ok(_) => {
                    let mut b = pending.pop_front().expect("front exists");
                    b.clear();
                    pool.push(b);
                }
                Err(e) => {
                    tracing::trace!(error = %e, "echo server send_copy failed");
                    let _ = t.disconnect();
                    return;
                }
            }
        }

        let alive = poll_fn(|cx| {
            // Free send buffers.
            match t.poll_send_completion(cx) {
                Poll::Ready(Ok(())) => return Poll::Ready(true),
                Poll::Ready(Err(e)) => {
                    tracing::trace!(error = %e, "echo server send cq error");
                    return Poll::Ready(false);
                }
                Poll::Pending => {}
            }

            // Receive requests and queue them for echo.
            let mut out = [RecvCompletion::default(); RECV_BATCH];
            match t.poll_recv(cx, &mut out) {
                Poll::Ready(Ok(n)) => {
                    for c in &out[..n] {
                        let mut b = pool.pop().unwrap_or_default();
                        b.extend_from_slice(&t.recv_buf(c.buf_idx)[..c.byte_len]);
                        let _ = t.repost_recv(c.buf_idx);
                        pending.push_back(b);
                    }
                    Poll::Ready(true)
                }
                Poll::Ready(Err(e)) => {
                    tracing::trace!(error = %e, "echo server recv cq error");
                    Poll::Ready(false)
                }
                Poll::Pending => {
                    if t.poll_disconnect(cx) {
                        Poll::Ready(false)
                    } else {
                        Poll::Pending
                    }
                }
            }
        })
        .await;

        if !alive {
            break;
        }
    }

    let _ = t.disconnect();
}

/// Run the raw-transport echo server: accept connections until `shutdown`
/// resolves, spawning an echo loop per connection.
pub async fn run_transport_echo_server<B, S>(
    builder: B,
    bind: SocketAddr,
    transport_label: &str,
    shutdown: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
    S: Future<Output = ()>,
{
    let listener = AsyncCmListener::bind(&bind)?;
    let local = listener.local_addr();
    eprintln!("Echo server listening on {local:?} (mode=echo, transport={transport_label})");

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            res = builder.accept(&listener) => {
                match res {
                    Ok(t) => { tokio::spawn(server_conn_loop(t)); }
                    Err(e) => tracing::warn!(error = %e, "echo accept failed"),
                }
            }
        }
    }

    eprintln!("Echo server draining");
    Ok(())
}

// ---------------------------------------------------------------------------
// Busy-poll read-ring echo (thread-per-core BusyPool)
// ---------------------------------------------------------------------------

/// Probe for the shared per-device verbs context that backs the pool's per-core
/// driver CQs.
///
/// librdmacm caches **one** `ibv_context` per device across cm_ids, so a
/// throwaway cm_id that merely *resolves* `addr` yields the context every QP
/// built on that device will share — which is exactly the context the pool's
/// shared CQs must live on. The returned cm_id must be kept alive for the pool's
/// lifetime (it owns that cached context).
pub(crate) async fn probe_device_context(
    addr: SocketAddr,
) -> Result<(AsyncCmId, Arc<Context>), Box<dyn std::error::Error>> {
    let probe = AsyncCmId::new(PortSpace::Tcp)?;
    probe.resolve_addr(None, &addr, 2000).await?;
    let ctx = probe
        .verbs_context()
        .ok_or("probe cm_id has no verbs context (device address did not resolve)")?;
    Ok((probe, ctx))
}

/// Run the busy-poll read-ring echo **client**: build a [`BusyPool`] over
/// `busy_cores` pinned cores, shard `connections` read-ring connections
/// round-robin across them, and drive each connection's pipelined echo loop
/// **on its owning core**.
///
/// Reuses [`client_conn_loop`] and the same [`BenchResult`] reporting as the
/// arm-park echo client (`run_transport_echo_client`), so busy-poll and arm-park
/// numbers are directly comparable — the only difference is the completion
/// driver and the runtime topology. `busy_cores` is the executor pool size (the
/// `threads` analogue), so `cpu_us_per_op` reads against a known number of
/// pinned cores.
#[allow(clippy::too_many_arguments)]
pub async fn run_read_ring_busy_client(
    addr: SocketAddr,
    config: ReadRingConfig,
    busy_cores: usize,
    connections: usize,
    in_flight: usize,
    payload: usize,
    warmup: u64,
    duration: u64,
    report: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cores = busy_cores.max(1);
    let (probe, ctx) = probe_device_context(addr).await?;

    let conns_per_core = connections.max(1).div_ceil(cores);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = BusyPool::with_config(ctx, &core_ids, &config, conns_per_core)?;
    eprintln!(
        "Busy-poll echo client: {cores} pinned core(s), {connections} read-ring connection(s) \
         ({conns_per_core}/core), in_flight={in_flight}"
    );

    let payload_buf: Arc<[u8]> = vec![b'x'; payload].into();
    let target = in_flight.max(1);
    let warmup_deadline = Instant::now() + Duration::from_secs(warmup);
    let bench_deadline = warmup_deadline + Duration::from_secs(duration);
    eprintln!("Warming up {warmup}s, then benchmarking {duration}s...");

    let sampler = spawn_resource_sampler(warmup_deadline, bench_deadline);

    // Shard the connections onto the pool. Each `connect_busy` setup *and* its
    // echo loop run on the owning core (co-location, §7.5); the JoinHandle
    // yields the per-connection latency histogram + error count.
    let mut handles = Vec::with_capacity(connections);
    for _ in 0..connections {
        let p = payload_buf.clone();
        let cfg = config.clone();
        let handle = pool
            .spawn_connect(addr, cfg, move |t| {
                client_conn_loop(t, p, target, warmup_deadline, bench_deadline)
            })
            .ok_or("busy pool refused a connection (per-core admission cap reached)")?;
        handles.push(handle);
    }

    let mut merged = new_histogram();
    let mut errors = 0u64;
    for (i, h) in handles.into_iter().enumerate() {
        match h.await {
            Ok(Ok((hist, errs))) => {
                errors += errs;
                merged.add(&hist).expect("merge histograms");
            }
            Ok(Err(e)) => {
                tracing::warn!(conn_id = i, error = %e, "busy echo connect_busy failed");
                errors += 1;
            }
            Err(e) => tracing::warn!(conn_id = i, error = %e, "busy echo client task panicked"),
        }
    }

    let (cpu_seconds, peak_rss) = sampler.await.unwrap_or((0.0, 0));

    report_result(
        &merged,
        "echo-busy",
        "read-ring",
        connections,
        cores,
        target,
        duration,
        payload,
        errors,
        cpu_seconds,
        peak_rss,
        report,
    );

    // Every connection's JoinHandle has been awaited (each transport `Drop`
    // handed its resources to its core's reclaim queue); now stop the drivers
    // (draining reclaim) and join the pinned threads before releasing the probe.
    pool.shutdown();
    drop(probe);
    Ok(())
}

/// Run the busy-poll read-ring echo **server**: build a [`BusyPool`] over
/// `busy_cores` pinned cores, accept exactly `connections` read-ring
/// connections (round-robin across cores, handshake serialized), and echo on
/// each connection's owning core until the peers disconnect or `shutdown`
/// resolves.
///
/// The device context is probed by resolving the server's own `bind` address
/// (it routes over the local RoCE device), so `bind` must be a concrete RDMA IP,
/// not the unspecified address.
pub async fn run_read_ring_busy_server<S>(
    bind: SocketAddr,
    config: ReadRingConfig,
    busy_cores: usize,
    connections: usize,
    shutdown: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: Future<Output = ()>,
{
    if bind.ip().is_unspecified() {
        return Err(format!(
            "busy-poll echo server needs a concrete --bind RDMA address to probe the device \
             context, but got the unspecified address {bind}; bind to the NIC's RoCE IP \
             (e.g. 10.0.1.5:50051)"
        )
        .into());
    }
    let cores = busy_cores.max(1);
    // Resolve our own bind address to obtain the shared device context (keep the
    // probe cm_id alive for the pool's lifetime).
    let (probe, ctx) = probe_device_context(bind).await?;

    let conns_per_core = connections.max(1).div_ceil(cores);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = BusyPool::with_config(ctx, &core_ids, &config, conns_per_core)?;

    let listener = Arc::new(AsyncCmListener::bind(&bind)?);
    let local = listener.local_addr();
    eprintln!(
        "Busy-poll echo server listening on {local:?}: {cores} pinned core(s), accepting \
         {connections} read-ring connection(s) ({conns_per_core}/core)"
    );

    // Accept exactly `connections` (serialized handshake), each echoing on its
    // owning core until its peer disconnects.
    let handles = pool
        .serve(listener, config, connections, server_conn_loop)
        .await;

    // Hold until every connection closes (clients finished) or a shutdown signal
    // arrives, then stop the drivers (draining reclaim) and join the threads.
    let all_closed = async {
        for h in handles {
            let _ = h.await;
        }
    };
    tokio::pin!(shutdown);
    tokio::select! {
        _ = all_closed => eprintln!("Busy-poll echo server: all connections closed"),
        _ = &mut shutdown => eprintln!("Busy-poll echo server: shutdown signal received, draining"),
    }

    pool.shutdown();
    drop(probe);
    Ok(())
}

// ---------------------------------------------------------------------------
// Thread-per-core arm-park read-ring echo (ArmParkPool)
// ---------------------------------------------------------------------------

/// Run the thread-per-core **arm-park** read-ring echo **client**: build an
/// [`ArmParkPool`] over `park_cores` pinned cores, shard `connections` read-ring
/// connections round-robin across them, and drive each connection's pipelined
/// echo loop **on its owning core**.
///
/// This is the interrupt-driven sibling of [`run_read_ring_busy_client`]: same
/// pinned thread-per-core topology and the same [`client_conn_loop`] +
/// [`BenchResult`] reporting, but each connection keeps its own arm-park CQ and
/// the core parks in `epoll_wait` when idle (0 % idle CPU) instead of a shared
/// busy-polled CQ. `park_cores` is the pool size (the `threads` analogue), so
/// `cpu_us_per_op` reads against a known number of pinned cores. Results are
/// labelled `echo-park` to separate them from `echo` (multi-thread arm-park) and
/// `echo-busy`.
#[allow(clippy::too_many_arguments)]
pub async fn run_read_ring_park_client(
    addr: SocketAddr,
    config: ReadRingConfig,
    park_cores: usize,
    connections: usize,
    in_flight: usize,
    payload: usize,
    warmup: u64,
    duration: u64,
    report: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cores = park_cores.max(1);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = ArmParkPool::new(&core_ids)?;
    eprintln!(
        "Thread-per-core arm-park echo client: {cores} pinned core(s), {connections} read-ring \
         connection(s), in_flight={in_flight}"
    );

    let payload_buf: Arc<[u8]> = vec![b'x'; payload].into();
    let target = in_flight.max(1);
    let warmup_deadline = Instant::now() + Duration::from_secs(warmup);
    let bench_deadline = warmup_deadline + Duration::from_secs(duration);
    eprintln!("Warming up {warmup}s, then benchmarking {duration}s...");

    let sampler = spawn_resource_sampler(warmup_deadline, bench_deadline);

    // Shard the connections onto the pool. Each `connect` setup *and* its echo
    // loop run on the owning core, so the transport's fds bind to that core's
    // reactor; the JoinHandle yields the per-connection histogram + error count.
    let mut handles = Vec::with_capacity(connections);
    for _ in 0..connections {
        let p = payload_buf.clone();
        let cfg = config.clone();
        handles.push(pool.spawn_connect(addr, cfg, move |t| {
            client_conn_loop(t, p, target, warmup_deadline, bench_deadline)
        }));
    }

    let mut merged = new_histogram();
    let mut errors = 0u64;
    for (i, h) in handles.into_iter().enumerate() {
        match h.await {
            Ok(Ok((hist, errs))) => {
                errors += errs;
                merged.add(&hist).expect("merge histograms");
            }
            Ok(Err(e)) => {
                tracing::warn!(conn_id = i, error = %e, "arm-park pool connect failed");
                errors += 1;
            }
            Err(e) => tracing::warn!(conn_id = i, error = %e, "arm-park echo client task panicked"),
        }
    }

    let (cpu_seconds, peak_rss) = sampler.await.unwrap_or((0.0, 0));

    report_result(
        &merged,
        "echo-park",
        "read-ring",
        connections,
        cores,
        target,
        duration,
        payload,
        errors,
        cpu_seconds,
        peak_rss,
        report,
    );

    // Every connection's JoinHandle has been awaited (each transport dropped,
    // arm-park teardown drained); now stop the runtimes and join the threads.
    pool.shutdown();
    Ok(())
}

/// Run the thread-per-core **arm-park** read-ring echo **server**: build an
/// [`ArmParkPool`] over `park_cores` pinned cores, accept exactly `connections`
/// read-ring connections (round-robin across cores, handshake serialized), and
/// echo on each connection's owning core until the peers disconnect or
/// `shutdown` resolves.
///
/// The listener is bound on the calling (orchestration) runtime; its fd
/// readiness wakes the on-core accept tasks via cross-runtime wakers, while each
/// accepted transport's fds bind to its owning core (so the data path is
/// core-local). Unlike the busy server, no device-context probe is needed, so
/// `bind` may be any address the CM can bind (including the unspecified one).
pub async fn run_read_ring_park_server<S>(
    bind: SocketAddr,
    config: ReadRingConfig,
    park_cores: usize,
    connections: usize,
    shutdown: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: Future<Output = ()>,
{
    let cores = park_cores.max(1);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = ArmParkPool::new(&core_ids)?;

    let listener = Arc::new(AsyncCmListener::bind(&bind)?);
    let local = listener.local_addr();
    eprintln!(
        "Thread-per-core arm-park echo server listening on {local:?}: {cores} pinned core(s), \
         accepting {connections} read-ring connection(s)"
    );

    // Accept exactly `connections` (serialized handshake), each echoing on its
    // owning core until its peer disconnects.
    let handles = pool
        .serve(listener, config, connections, server_conn_loop)
        .await;

    // Hold until every connection closes (clients finished) or a shutdown signal
    // arrives, then stop the runtimes and join the threads.
    let all_closed = async {
        for h in handles {
            let _ = h.await;
        }
    };
    tokio::pin!(shutdown);
    tokio::select! {
        _ = all_closed => eprintln!("arm-park pool server: all connections closed"),
        _ = &mut shutdown => eprintln!("arm-park pool server: shutdown signal received, draining"),
    }

    pool.shutdown();
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw TCP echo baseline
// ---------------------------------------------------------------------------

/// TCP echo client for one connection, with `target` requests outstanding.
///
/// Uses a semaphore to bound outstanding requests and a shared FIFO of send
/// timestamps to match fixed-size responses back to requests. The server echoes
/// bytes verbatim, so each `payload`-sized read is exactly one response.
async fn tcp_client_conn(
    stream: TcpStream,
    payload: Arc<[u8]>,
    target: usize,
    warmup_deadline: Instant,
    bench_deadline: Instant,
) -> (Histogram<u64>, u64) {
    let (mut rd, mut wr) = stream.into_split();
    let plen = payload.len();
    let sem = Arc::new(Semaphore::new(target));
    let times: Arc<Mutex<VecDeque<Instant>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(target + 2)));

    let sem_w = sem.clone();
    let times_w = times.clone();
    let writer = tokio::spawn(async move {
        let mut errs = 0u64;
        while Instant::now() < bench_deadline {
            // Bound outstanding requests; a permit is returned by the reader.
            let permit = match sem_w.acquire().await {
                Ok(p) => p,
                Err(_) => break, // semaphore closed => stop
            };
            permit.forget();
            times_w.lock().await.push_back(Instant::now());
            if wr.write_all(&payload[..]).await.is_err() {
                errs += 1;
                break;
            }
        }
        errs
    });

    let mut hist = new_histogram();
    let mut errs = 0u64;
    let mut buf = vec![0u8; plen];
    let deadline = tokio::time::Instant::from_std(bench_deadline);
    loop {
        if Instant::now() >= bench_deadline {
            break;
        }
        match tokio::time::timeout_at(deadline, rd.read_exact(&mut buf)).await {
            Err(_elapsed) => break,
            Ok(Ok(_)) => {
                let done = Instant::now();
                let start = times.lock().await.pop_front();
                if let Some(start) = start
                    && start >= warmup_deadline
                {
                    let us = done.saturating_duration_since(start).as_micros() as u64;
                    let _ = hist.record(us);
                }
                sem.add_permits(1);
            }
            Ok(Err(_)) => {
                errs += 1;
                break;
            }
        }
    }

    // Stop the writer and collect its error count.
    sem.close();
    if let Ok(werrs) = writer.await {
        errs += werrs;
    }
    (hist, errs)
}

/// Run the raw-TCP echo client (baseline for the RDMA transports).
#[allow(clippy::too_many_arguments)]
pub async fn run_tcp_echo_client(
    addr: SocketAddr,
    connections: usize,
    connect_retries: u32,
    in_flight: usize,
    payload: usize,
    warmup: u64,
    duration: u64,
    threads: usize,
    report: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let payload_buf: Arc<[u8]> = vec![b'x'; payload].into();

    let mut streams = Vec::with_capacity(connections);
    for _ in 0..connections {
        let mut tries = 0u32;
        let stream = loop {
            match tokio::time::timeout(CONNECT_ATTEMPT_TIMEOUT, TcpStream::connect(addr)).await {
                Ok(Ok(s)) => break s,
                Ok(Err(e)) => {
                    if !backoff_or_giveup("tcp", tries, connect_retries, &e).await {
                        return Err(e.into());
                    }
                }
                Err(_) => {
                    let e = format!("connect timed out after {CONNECT_ATTEMPT_TIMEOUT:?}");
                    if !backoff_or_giveup("tcp", tries, connect_retries, &e).await {
                        return Err(e.into());
                    }
                }
            }
            tries += 1;
        };
        stream.set_nodelay(true)?;
        streams.push(stream);
    }
    eprintln!(
        "Connected {connections} echo clients to {addr} \
         (mode=echo, transport=tcp, in_flight={in_flight}, threads={threads})"
    );

    let target = in_flight.max(1);
    let warmup_deadline = Instant::now() + Duration::from_secs(warmup);
    let bench_deadline = warmup_deadline + Duration::from_secs(duration);
    eprintln!("Warming up {warmup}s, then benchmarking {duration}s...");

    let sampler = spawn_resource_sampler(warmup_deadline, bench_deadline);

    let mut handles = Vec::with_capacity(connections);
    for stream in streams {
        let p = payload_buf.clone();
        handles.push(tokio::spawn(tcp_client_conn(
            stream,
            p,
            target,
            warmup_deadline,
            bench_deadline,
        )));
    }

    let mut merged = new_histogram();
    let mut errors = 0u64;
    for (i, h) in handles.into_iter().enumerate() {
        match h.await {
            Ok((hist, errs)) => {
                errors += errs;
                merged.add(&hist).expect("merge histograms");
            }
            Err(e) => tracing::warn!(conn_id = i, error = %e, "tcp echo client task panicked"),
        }
    }

    let (cpu_seconds, peak_rss) = sampler.await.unwrap_or((0.0, 0));

    report_result(
        &merged,
        "echo",
        "tcp",
        connections,
        threads,
        target,
        duration,
        payload,
        errors,
        cpu_seconds,
        peak_rss,
        report,
    );
    Ok(())
}

/// Echo loop for one accepted TCP connection: read bytes, write them back.
async fn tcp_echo_conn(mut stream: TcpStream) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break, // peer closed
            Ok(n) => {
                if stream.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Run the raw-TCP echo server: accept until `shutdown` resolves, spawning a
/// byte-echo loop per connection.
pub async fn run_tcp_echo_server<S>(
    bind: SocketAddr,
    shutdown: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: Future<Output = ()>,
{
    let listener = TcpListener::bind(bind).await?;
    eprintln!("Echo server listening on {bind} (mode=echo, transport=tcp)");

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            res = listener.accept() => {
                match res {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        tokio::spawn(tcp_echo_conn(stream));
                    }
                    Err(e) => tracing::warn!(error = %e, "tcp echo accept failed"),
                }
            }
        }
    }

    eprintln!("Echo server draining");
    Ok(())
}
