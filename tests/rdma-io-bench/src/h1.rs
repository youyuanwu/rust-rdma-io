//! HTTP/1.1 benchmark modes: `rh1` (HTTP/1.1 over RDMA) and `tcp1` (HTTP/1.1
//! over a kernel socket, the non-RDMA baseline).
//!
//! Uses hyper's low-level `http1` connection API over the same OpenSSL/TLS layer
//! as [`grpc`](crate::grpc)'s `rh2`, carried on the raw RDMA byte stream (`rh1`)
//! or a kernel TCP socket (`tcp1`). HTTP/1.1 has no stream multiplexing, so each
//! connection issues one request at a time; scale offered load with
//! `--connections`.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use openssl::ssl::{Ssl, SslAcceptor, SslConnector};
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::transport::TransportBuilder;
use rdma_io_tonic::{RdmaIncoming, TokioRdmaStream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_openssl::SslStream;

use crate::common::{ClientOpts, ServerOpts, connect_with_retry, transport_setup_error};
use crate::metrics::{BenchMetrics, spawn_resource_sampler};
use crate::report::BenchResult;
use crate::tls_common;

/// HTTP/1.1 client `SendRequest` handle over the shared body type.
type H1Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

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
