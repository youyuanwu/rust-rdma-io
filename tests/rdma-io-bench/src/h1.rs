//! HTTP/1.1 benchmark modes: `rh1` (HTTP/1.1 over RDMA) and `tcp1` (HTTP/1.1
//! over a kernel socket, the non-RDMA baseline).
//!
//! Uses hyper's low-level `http1` connection API over the same OpenSSL/TLS layer
//! as [`grpc`](crate::grpc)'s `rh2`, carried on the raw RDMA byte stream (`rh1`)
//! or a kernel TCP socket (`tcp1`). HTTP/1.1 has no stream multiplexing, so each
//! connection issues one request at a time; scale offered load with
//! `--connections`.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use hdrhistogram::Histogram;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use openssl::ssl::{Ssl, SslAcceptor, SslConnector};
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::read_ring_transport::{ReadRingConfig, ReadRingTransport};
use rdma_io::transport::TransportBuilder;
use rdma_io_busy::{ArmParkPool, BusyPool};
use rdma_io_tonic::{RdmaIncoming, TokioRdmaStream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_openssl::SslStream;
use tokio_util::sync::CancellationToken;

use crate::common::{ClientOpts, ServerOpts, connect_with_retry, transport_setup_error};
use crate::metrics::{BenchMetrics, run_readiness_barrier, spawn_resource_sampler};
use crate::report::BenchResult;
use crate::tls_common;

/// HTTP/1.1 client `SendRequest` handle over the shared body type.
type H1Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

/// Histogram matching the echo modes' bounds (1µs..60s, 3 sig figs), so the
/// thread-per-core HTTP/1.1 latencies line up with the other benchmark outputs.
fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("create histogram")
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// RDMA HTTP/1.1 client (`--mode rh1`): hyper `http1` over the same OpenSSL/TLS
/// layer as `rh2`, carried on the raw RDMA byte stream.
pub async fn run_h1_client<B>(
    builder: B,
    opts: &ClientOpts,
    transport_label: &str,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let connector = tls_common::build_connector_h1(&opts.cert);
    let addr = opts.connect;

    let mut senders = Vec::with_capacity(opts.connections);
    for i in 0..opts.connections {
        let sender = connect_with_retry(&format!("rh1 conn {i}"), Duration::from_secs(15), || {
            let builder = builder.clone();
            let connector = connector.clone();
            let transport_name = transport_label.to_string();
            async move {
                let transport = builder
                    .connect(&addr)
                    .await
                    .map_err(|e| transport_setup_error(&transport_name, e))?;
                let rdma = TokioRdmaStream::new(AsyncRdmaStream::new(transport));
                h1_connect(rdma, &connector).await
            }
        })
        .await?;
        senders.push(sender);
    }

    eprintln!(
        "Connected {} h1 clients to {} (mode=rh1, transport={}, threads={})",
        opts.connections, addr, transport_label, opts.threads
    );

    run_h1_clients(opts, senders, "rh1", transport_label).await
}

/// TCP HTTP/1.1 baseline client (`--mode tcp1`): the same hyper `http1` +
/// OpenSSL stack over a kernel TCP socket, so the only variable vs `rh1` is the
/// byte transport.
pub async fn run_tcp1_client(opts: &ClientOpts) -> Result<(), Box<dyn std::error::Error>> {
    let connector = tls_common::build_connector_h1(&opts.cert);
    let addr = opts.connect;

    let mut senders = Vec::with_capacity(opts.connections);
    for i in 0..opts.connections {
        let sender = connect_with_retry(&format!("tcp1 conn {i}"), Duration::from_secs(15), || {
            let connector = connector.clone();
            async move {
                let tcp = tokio::net::TcpStream::connect(addr).await?;
                // Disable Nagle: this is a small-message request/response
                // workload, so coalescing delays would inflate latency (and
                // RDMA has no Nagle equivalent) — keep the baseline fair.
                tcp.set_nodelay(true)?;
                h1_connect(tcp, &connector).await
            }
        })
        .await?;
        senders.push(sender);
    }

    eprintln!(
        "Connected {} h1 clients to {} (mode=tcp1, threads={})",
        opts.connections, addr, opts.threads
    );

    run_h1_clients(opts, senders, "tcp1", "tcp").await
}

/// TLS-wrap an async byte stream (RDMA or TCP), perform the OpenSSL client
/// handshake, then complete the hyper HTTP/1.1 handshake and spawn the
/// connection driver. Returns the `SendRequest` used to issue requests.
async fn h1_connect<S>(
    stream: S,
    connector: &SslConnector,
) -> Result<H1Sender, Box<dyn std::error::Error>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ssl = connector.configure()?.into_ssl("rdma-bench")?;
    let mut tls = SslStream::new(ssl, stream)?;
    Pin::new(&mut tls).connect().await?;

    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::trace!(error = %e, "h1 connection closed");
        }
    });
    Ok(sender)
}

/// Issue one HTTP/1.1 request and fully drain the response body (required for
/// keep-alive connection reuse).
async fn send_h1(
    sender: &mut H1Sender,
    req: hyper::Request<Full<Bytes>>,
) -> Result<(), hyper::Error> {
    sender.ready().await?;
    let resp = sender.send_request(req).await?;
    resp.into_body().collect().await?;
    Ok(())
}

/// Shared warmup + benchmark + report loop for the HTTP/1.1 modes. Each
/// connection runs one sequential request loop (h1 has no multiplexing), so the
/// reported pipeline depth is always 1 and load scales with `--connections`.
async fn run_h1_clients(
    opts: &ClientOpts,
    senders: Vec<H1Sender>,
    mode: &str,
    transport: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if opts.in_flight > 1 {
        eprintln!(
            "note: {mode} (HTTP/1.1) has no request multiplexing; --in-flight {} is treated as 1 \
             (scale offered load with --connections)",
            opts.in_flight
        );
    }

    let authority = format!("{}:{}", opts.connect.ip(), opts.connect.port());
    let payload = "x".repeat(opts.payload);

    // CPU + peak-RSS sampling window is [warmup_deadline, bench_deadline] so the
    // reported CPU-per-op excludes connection setup and warmup (mirrors rh2/echo).
    let warmup_deadline = Instant::now() + Duration::from_secs(opts.warmup);
    let bench_deadline = warmup_deadline + Duration::from_secs(opts.duration);
    let sampler = spawn_resource_sampler(warmup_deadline, bench_deadline);

    let metrics = BenchMetrics::new(senders.len());

    eprintln!(
        "Benchmarking for {}s (1 request per connection, {} connections)...",
        opts.duration,
        senders.len()
    );

    let mut handles = Vec::new();
    for (id, mut sender) in senders.into_iter().enumerate() {
        let m = metrics.clone();
        let body = payload.clone().into_bytes();
        let authority = authority.clone();
        handles.push(tokio::spawn(async move {
            while Instant::now() < bench_deadline {
                let start = Instant::now();
                let req = match hyper::Request::builder()
                    .method(hyper::Method::POST)
                    .uri("/")
                    .header(hyper::header::HOST, authority.as_str())
                    .body(Full::new(Bytes::from(body.clone())))
                {
                    Ok(req) => req,
                    Err(_) => {
                        m.record_error();
                        continue;
                    }
                };
                match send_h1(&mut sender, req).await {
                    Ok(()) => {
                        if start >= warmup_deadline {
                            m.record(id, start.elapsed()).await;
                        }
                    }
                    Err(e) => {
                        tracing::trace!(id, error = %e, "h1 request failed");
                        if start >= warmup_deadline {
                            m.record_error();
                        }
                    }
                }
            }
        }));
    }

    for (i, h) in handles.into_iter().enumerate() {
        if let Err(e) = h.await {
            tracing::warn!(task = i, error = %e, "h1 bench task failed");
        }
    }

    let (cpu_seconds, peak_rss_kb) = sampler.await.unwrap_or((0.0, 0));
    let hist = metrics.merged_histogram().await;
    let result = BenchResult::from_histogram(
        &hist,
        mode,
        transport,
        opts.connections,
        opts.threads,
        1, // HTTP/1.1: one request outstanding per connection
        opts.duration,
        opts.payload,
        metrics.total_errors.load(Ordering::Relaxed),
    )
    .with_resource_usage(cpu_seconds, peak_rss_kb);

    match opts.report.as_str() {
        "json" => result.print_json(),
        _ => result.print_text(),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// RDMA HTTP/1.1 server (`--mode rh1`): serves hyper `http1` over the same
/// OpenSSL/TLS layer as `rh2`, on the raw RDMA byte stream. Hands each accepted
/// connection to [`serve_h1_conn`].
pub async fn run_h1_server<B>(
    builder: B,
    opts: &ServerOpts,
    transport_label: &str,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let acceptor = Arc::new(tls_common::build_acceptor_h1(&opts.cert, &opts.key));

    let listener = AsyncCmListener::bind(&opts.bind)?;
    let local_addr = listener.local_addr();
    let mut incoming = RdmaIncoming::new(listener, builder);

    eprintln!(
        "Benchmark server listening on {:?} (mode=rh1, transport={})",
        local_addr, transport_label
    );

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            conn = incoming.next() => match conn {
                Some(Ok(stream)) => {
                    let acceptor = acceptor.clone();
                    tokio::spawn(serve_h1_conn(stream, acceptor));
                }
                Some(Err(e)) => tracing::warn!(error = %e, "rh1 accept failed"),
                None => break,
            }
        }
    }

    Ok(())
}

/// TCP HTTP/1.1 baseline server (`--mode tcp1`): the same hyper `http1` +
/// OpenSSL stack over a kernel TCP listener, so the only variable vs `rh1` is
/// the transport.
pub async fn run_tcp1_server(
    opts: &ServerOpts,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let acceptor = Arc::new(tls_common::build_acceptor_h1(&opts.cert, &opts.key));

    let listener = tokio::net::TcpListener::bind(opts.bind).await?;

    eprintln!("Benchmark server listening on {} (mode=tcp1)", opts.bind);

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    let acceptor = acceptor.clone();
                    tokio::spawn(serve_h1_conn(stream, acceptor));
                }
                Err(e) => tracing::warn!(error = %e, "tcp1 accept failed"),
            }
        }
    }

    Ok(())
}

/// TLS-accept an incoming byte stream (RDMA or TCP) and serve it as an HTTP/1.1
/// connection whose handler echoes each request body back as the response body.
async fn serve_h1_conn<S>(stream: S, acceptor: Arc<SslAcceptor>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ssl = match Ssl::new(acceptor.context()) {
        Ok(ssl) => ssl,
        Err(e) => {
            tracing::trace!(error = %e, "h1 ssl init failed");
            return;
        }
    };
    let mut tls = match SslStream::new(ssl, stream) {
        Ok(tls) => tls,
        Err(e) => {
            tracing::trace!(error = %e, "h1 ssl stream init failed");
            return;
        }
    };
    if let Err(e) = Pin::new(&mut tls).accept().await {
        tracing::trace!(error = %e, "h1 tls handshake failed");
        return;
    }

    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tls), service_fn(h1_echo))
        .await
    {
        tracing::trace!(error = %e, "h1 connection error");
    }
}

/// HTTP/1.1 echo handler: returns the request body as the response body so the
/// same payload traverses the transport in both directions (bandwidth parity
/// with the gRPC `say_hello` round-trip).
async fn h1_echo(
    req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, std::convert::Infallible> {
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    Ok(hyper::Response::new(Full::new(body)))
}

// ---------------------------------------------------------------------------
// Thread-per-core HTTP/1.1 (`rh1-busy` / `rh1-park`)
// ---------------------------------------------------------------------------
//
// The `rh1` client/server above run the whole hyper HTTP/1.1 + TLS stack on a
// shared multi-thread tokio runtime, so a connection's request loop, its TLS
// state, and its RDMA completion handling can migrate across cores (tokio
// work-stealing). These two variants instead pin each connection to a core for
// life — the read-ring transport, the `AsyncRdmaStream`, the OpenSSL session,
// and the hyper `http1` driver all run on one core's `current_thread` runtime —
// mirroring the `echo-busy` / `echo-park` topologies:
//
// - `rh1-busy` drives completions with a shared-CQ [`BusyPool`] (busy-poll, one
//   reaper per core, 100 % core even when idle, lowest latency).
// - `rh1-park` drives completions with an [`ArmParkPool`] (per-connection armed
//   CQ, the core parks in `epoll_wait` when idle, 0 % idle CPU).
//
// HTTP/1.1 keeps exactly one request in flight per connection, so offered load
// scales with `--connections`; `--threads` is the number of pinned cores across
// which the connections are sharded round-robin.

/// Per-connection HTTP/1.1 request loop for the thread-per-core modes: complete
/// the TLS + hyper handshake over the already-wrapped `stream`, then issue
/// sequential requests until `bench_deadline`, recording post-warmup latencies.
/// Runs entirely on the owning core (the caller invokes it inside a pool
/// closure), so the hyper connection driver it spawns stays core-local. Returns
/// the connection's latency histogram and error count.
async fn h1_conn_loop<S>(
    stream: S,
    connector: SslConnector,
    authority: String,
    body: Vec<u8>,
    warmup_deadline: Instant,
    bench_deadline: Instant,
) -> (Histogram<u64>, u64)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut hist = new_histogram();
    let mut sender = match h1_connect(stream, &connector).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "thread-per-core h1 handshake failed");
            return (hist, 1);
        }
    };

    let mut errors = 0u64;
    while Instant::now() < bench_deadline {
        let start = Instant::now();
        let req = match hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri("/")
            .header(hyper::header::HOST, authority.as_str())
            .body(Full::new(Bytes::from(body.clone())))
        {
            Ok(req) => req,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        match send_h1(&mut sender, req).await {
            Ok(()) => {
                if start >= warmup_deadline {
                    let us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
                    let _ = hist.record(us.max(1));
                }
            }
            Err(e) => {
                tracing::trace!(error = %e, "h1 request failed");
                if start >= warmup_deadline {
                    errors += 1;
                }
            }
        }
    }
    (hist, errors)
}

/// Emit the merged thread-per-core HTTP/1.1 result in the same shape as `rh1`
/// (`in_flight` is always 1; `threads` reports the pinned-core count).
#[allow(clippy::too_many_arguments)]
fn report_h1_thread_per_core(
    merged: &Histogram<u64>,
    mode: &str,
    connections: usize,
    cores: usize,
    duration: u64,
    payload: usize,
    errors: u64,
    cpu_seconds: f64,
    peak_rss_kb: u64,
    report: &str,
) {
    let result = BenchResult::from_histogram(
        merged,
        mode,
        "read-ring",
        connections,
        cores,
        1,
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

/// Run the busy-poll HTTP/1.1 **client** (`--mode rh1-busy`): build a
/// [`BusyPool`] over `busy_cores` pinned cores, shard `connections` read-ring
/// connections round-robin across them, and drive each connection's HTTP/1.1
/// request loop **on its owning core**. Reports as `rh1-busy` / `read-ring`.
#[allow(clippy::too_many_arguments)]
pub async fn run_h1_busy_client(
    addr: SocketAddr,
    config: ReadRingConfig,
    busy_cores: usize,
    connections: usize,
    payload: usize,
    warmup: u64,
    duration: u64,
    cert: &Path,
    report: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cores = busy_cores.max(1);
    let (probe, ctx) = crate::echo::probe_device_context(addr).await?;

    let conns_per_core = connections.max(1).div_ceil(cores);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = BusyPool::with_config(ctx, &core_ids, &config, conns_per_core)?;
    eprintln!(
        "Busy-poll h1 client: {cores} pinned core(s), {connections} read-ring connection(s) \
         ({conns_per_core}/core), 1 request/connection"
    );

    let connector = tls_common::build_connector_h1(cert);
    let authority = format!("{}:{}", addr.ip(), addr.port());
    let body = "x".repeat(payload).into_bytes();
    eprintln!("Setting up {connections} connection(s) before the timed window...");

    // Readiness barrier: every connection finishes RDMA setup + TLS/hyper
    // handshake, reports ready, and blocks until all are ready before the shared
    // warmup/bench clock and the CPU/RSS sampler start — so setup time never
    // biases busy-vs-park (design §12 / §13). Each request loop runs on its core.
    let outcome = run_readiness_barrier(connections, warmup, duration, |ready, gate| {
        let connector = connector.clone();
        let authority = authority.clone();
        let body = body.clone();
        let cfg = config.clone();
        pool.spawn_connect(addr, cfg, move |t| async move {
            ready.report().await;
            let (warmup_deadline, bench_deadline) = gate.wait().await;
            let rdma = TokioRdmaStream::new(AsyncRdmaStream::new(t));
            h1_conn_loop(
                rdma,
                connector,
                authority,
                body,
                warmup_deadline,
                bench_deadline,
            )
            .await
        })
    })
    .await;

    report_h1_thread_per_core(
        &outcome.merged,
        "rh1-busy",
        connections,
        cores,
        duration,
        payload,
        outcome.errors,
        outcome.cpu_seconds,
        outcome.peak_rss_kb,
        report,
    );

    pool.shutdown();
    drop(probe);
    Ok(())
}

/// Run the thread-per-core arm-park HTTP/1.1 **client** (`--mode rh1-park`):
/// same pinned-per-core topology as [`run_h1_busy_client`], but driven through
/// an interrupt-armed [`ArmParkPool`] (per-connection armed CQ, cores park when
/// idle). Reports as `rh1-park` / `read-ring`.
#[allow(clippy::too_many_arguments)]
pub async fn run_h1_park_client(
    addr: SocketAddr,
    config: ReadRingConfig,
    park_cores: usize,
    connections: usize,
    payload: usize,
    warmup: u64,
    duration: u64,
    cert: &Path,
    report: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cores = park_cores.max(1);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = ArmParkPool::new(&core_ids)?;
    eprintln!(
        "Thread-per-core arm-park h1 client: {cores} pinned core(s), {connections} read-ring \
         connection(s), 1 request/connection"
    );

    let connector = tls_common::build_connector_h1(cert);
    let authority = format!("{}:{}", addr.ip(), addr.port());
    let body = "x".repeat(payload).into_bytes();
    eprintln!("Setting up {connections} connection(s) before the timed window...");

    // Readiness barrier (see `run_h1_busy_client`): start the shared clock +
    // sampler only after every connection's setup completes (design §12 / §13).
    let outcome = run_readiness_barrier(connections, warmup, duration, |ready, gate| {
        let connector = connector.clone();
        let authority = authority.clone();
        let body = body.clone();
        let cfg = config.clone();
        Some(pool.spawn_connect(addr, cfg, move |t| async move {
            ready.report().await;
            let (warmup_deadline, bench_deadline) = gate.wait().await;
            let rdma = TokioRdmaStream::new(AsyncRdmaStream::new(t));
            h1_conn_loop(
                rdma,
                connector,
                authority,
                body,
                warmup_deadline,
                bench_deadline,
            )
            .await
        }))
    })
    .await;

    report_h1_thread_per_core(
        &outcome.merged,
        "rh1-park",
        connections,
        cores,
        duration,
        payload,
        outcome.errors,
        outcome.cpu_seconds,
        outcome.peak_rss_kb,
        report,
    );

    pool.shutdown();
    Ok(())
}

/// Serve one accepted read-ring transport as a thread-per-core HTTP/1.1
/// connection: wrap it in an [`AsyncRdmaStream`], then run the same TLS-accept +
/// hyper `http1` echo path as [`serve_h1_conn`], on the owning core.
async fn serve_h1_transport(transport: ReadRingTransport, acceptor: Arc<SslAcceptor>) {
    let rdma = TokioRdmaStream::new(AsyncRdmaStream::new(transport));
    serve_h1_conn(rdma, acceptor).await;
}

/// Run the busy-poll HTTP/1.1 **server** (`--mode rh1-busy`): build a
/// [`BusyPool`] over `busy_cores` pinned cores, accept exactly `connections`
/// read-ring connections (round-robin, handshake serialized), and serve each on
/// its owning core until the peers disconnect or `shutdown` resolves. `bind`
/// must be a concrete RDMA IP (the pool probes the device context from it).
pub async fn run_h1_busy_server<Sh>(
    bind: SocketAddr,
    config: ReadRingConfig,
    busy_cores: usize,
    connections: usize,
    cert: &Path,
    key: &Path,
    shutdown: Sh,
) -> Result<(), Box<dyn std::error::Error>>
where
    Sh: Future<Output = ()>,
{
    if bind.ip().is_unspecified() {
        return Err(format!(
            "busy-poll h1 server needs a concrete --bind RDMA address to probe the device \
             context, but got the unspecified address {bind}; bind to the NIC's RoCE IP \
             (e.g. 10.0.1.5:50051)"
        )
        .into());
    }
    let cores = busy_cores.max(1);
    let (probe, ctx) = crate::echo::probe_device_context(bind).await?;

    let conns_per_core = connections.max(1).div_ceil(cores);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = BusyPool::with_config(ctx, &core_ids, &config, conns_per_core)?;

    let acceptor = Arc::new(tls_common::build_acceptor_h1(cert, key));
    let listener = Arc::new(AsyncCmListener::bind(&bind)?);
    let local = listener.local_addr();
    eprintln!(
        "Busy-poll h1 server listening on {local:?}: {cores} pinned core(s), accepting \
         {connections} read-ring connection(s) ({conns_per_core}/core)"
    );

    // Serve each connection cooperatively cancellable: its hyper HTTP/1.1 driver
    // races a shared cancellation token, so a shutdown signal makes every
    // connection return (dropping its transport → reclaim) — the
    // close-before-shutdown contract (review #3), via a token rather than a blunt
    // task abort.
    let cancel = CancellationToken::new();
    let serve_cancel = cancel.clone();
    let handles = pool
        .serve(listener, config, connections, move |t| {
            let cancel = serve_cancel.clone();
            let acceptor = acceptor.clone();
            async move {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = serve_h1_transport(t, acceptor) => {}
                }
            }
        })
        .await;

    let mut all_closed = Box::pin(async move {
        for h in handles {
            let _ = h.await;
        }
    });
    tokio::pin!(shutdown);
    tokio::select! {
        _ = &mut all_closed => eprintln!("Busy-poll h1 server: all connections closed"),
        _ = &mut shutdown => {
            eprintln!("Busy-poll h1 server: shutdown signal received, draining");
            cancel.cancel();
            all_closed.await;
        }
    }

    pool.shutdown();
    drop(probe);
    Ok(())
}

/// Run the thread-per-core arm-park HTTP/1.1 **server** (`--mode rh1-park`):
/// same pinned-per-core accept/serve topology as [`run_h1_busy_server`], but
/// driven through an interrupt-armed [`ArmParkPool`]. No device-context probe is
/// needed, so `bind` may be any address the CM can bind.
pub async fn run_h1_park_server<Sh>(
    bind: SocketAddr,
    config: ReadRingConfig,
    park_cores: usize,
    connections: usize,
    cert: &Path,
    key: &Path,
    shutdown: Sh,
) -> Result<(), Box<dyn std::error::Error>>
where
    Sh: Future<Output = ()>,
{
    let cores = park_cores.max(1);
    let core_ids: Vec<usize> = (0..cores).collect();
    let pool = ArmParkPool::new(&core_ids)?;

    let acceptor = Arc::new(tls_common::build_acceptor_h1(cert, key));
    let listener = Arc::new(AsyncCmListener::bind(&bind)?);
    let local = listener.local_addr();
    eprintln!(
        "Thread-per-core arm-park h1 server listening on {local:?}: {cores} pinned core(s), \
         accepting {connections} read-ring connection(s)"
    );

    let handles = pool
        .serve(listener, config, connections, move |t| {
            serve_h1_transport(t, acceptor.clone())
        })
        .await;

    let all_closed = async {
        for h in handles {
            let _ = h.await;
        }
    };
    tokio::pin!(shutdown);
    tokio::select! {
        _ = all_closed => eprintln!("arm-park h1 server: all connections closed"),
        _ = &mut shutdown => eprintln!("arm-park h1 server: shutdown signal received, draining"),
    }

    pool.shutdown();
    Ok(())
}
