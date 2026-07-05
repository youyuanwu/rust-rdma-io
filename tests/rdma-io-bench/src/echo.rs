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
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::transport::{RecvCompletion, Transport, TransportBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};

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

/// Cumulative user+system CPU time of this process (summed over all threads),
/// in seconds, read from `/proc/self/stat`. Fields 14 (`utime`) and 15
/// (`stime`) are in clock ticks; Linux `USER_HZ` is 100 on these VMs.
fn process_cpu_seconds() -> f64 {
    const USER_HZ: f64 = 100.0;
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // The comm field (2nd) is wrapped in parens and may itself contain spaces
    // or parens, so split on the final ')': everything after it is space-
    // separated starting at field 3 (state).
    if let Some((_, rest)) = stat.rsplit_once(')') {
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // utime = field 14 -> index 11, stime = field 15 -> index 12.
        if fields.len() > 12 {
            let utime: u64 = fields[11].parse().unwrap_or(0);
            let stime: u64 = fields[12].parse().unwrap_or(0);
            return (utime + stime) as f64 / USER_HZ;
        }
    }
    0.0
}

/// Peak resident set size of this process in kilobytes (`VmHWM` high-water
/// mark), read from `/proc/self/status`.
fn peak_rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("VmHWM:") {
            return v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// Spawn a task that measures this process's CPU consumption strictly over the
/// `[warmup_deadline, bench_deadline]` window plus the peak RSS, so the reported
/// CPU-per-op excludes connection setup and warmup. Returns `(cpu_seconds,
/// peak_rss_kb)`.
fn spawn_resource_sampler(
    warmup_deadline: Instant,
    bench_deadline: Instant,
) -> tokio::task::JoinHandle<(f64, u64)> {
    tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(warmup_deadline)).await;
        let cpu_start = process_cpu_seconds();
        tokio::time::sleep_until(tokio::time::Instant::from_std(bench_deadline)).await;
        let cpu_end = process_cpu_seconds();
        ((cpu_end - cpu_start).max(0.0), peak_rss_kb())
    })
}

/// Print an assembled [`BenchResult`] in the requested format.
#[allow(clippy::too_many_arguments)]
fn report_result(
    hist: &Histogram<u64>,
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
        "echo",
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
