//! gRPC (tonic) benchmark modes: `rh2` (HTTP/2 over RDMA), `rh3` (HTTP/3 over
//! QUIC/RDMA), and `tcp` (HTTP/2 over a kernel socket, the non-RDMA baseline).
//!
//! Each function takes the transport builder (already selected/constructed by
//! the binary) plus a [`ClientOpts`]/[`ServerOpts`] bundle, mirroring how
//! [`echo`](crate::echo) is factored out of the client/server binaries.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rdma_io::async_cm::AsyncCmListener;
use rdma_io::transport::TransportBuilder;
use rdma_io_quinn::RdmaUdpSocket;
use rdma_io_tonic::RdmaIncoming;
use rdma_io_tonic::tls::RdmaTransport;
use tonic::transport::{Endpoint, Server};
use tonic::{Request, Response, Status};

use crate::common::{ClientOpts, ServerOpts, connect_with_retry, transport_setup_error};
use crate::greeter::greeter_client::GreeterClient;
use crate::greeter::greeter_server::{Greeter, GreeterServer};
use crate::greeter::{HelloReply, HelloRequest};
use crate::metrics::{BenchMetrics, spawn_resource_sampler};
use crate::report::BenchResult;
use crate::{h3_common, tls_common};

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// RDMA HTTP/2 gRPC client (`--mode rh2`): tonic-tls + OpenSSL over the RDMA
/// byte stream.
pub async fn run_rh2_client<B>(
    builder: B,
    opts: &ClientOpts,
    transport_label: &str,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let uri = format!("https://{}:{}", opts.connect.ip(), opts.connect.port());
    let ssl_connector = tls_common::build_connector(&opts.cert);
    let transport = RdmaTransport::new(builder);

    // Create connections (retry: read-ring's RDMA-CM is flaky under connect
    // churn on MANA — see connect_with_retry).
    let mut clients = Vec::with_capacity(opts.connections);
    for i in 0..opts.connections {
        let channel = connect_with_retry(&format!("rh2 conn {i}"), Duration::from_secs(15), || {
            let transport = transport.clone();
            let ssl_connector = ssl_connector.clone();
            let uri = uri.clone();
            async move {
                let connector = tonic_tls::openssl::TlsConnector::new(
                    transport,
                    ssl_connector,
                    "rdma-bench".to_string(),
                );
                Endpoint::from_shared(uri)?
                    .connect_with_connector(connector)
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
            }
        })
        .await?;
        clients.push(GreeterClient::new(channel));
    }

    eprintln!(
        "Connected {} clients to {} (mode=rh2, transport={}, threads={})",
        opts.connections, uri, transport_label, opts.threads
    );

    run_channel_clients(opts, clients, "rh2", transport_label).await
}

/// TCP baseline gRPC client (`--mode tcp`): tonic-tls + OpenSSL over a standard
/// TCP socket (no RDMA).
///
/// Uses the same TLS/gRPC stack as `rh2` so the only variable is the underlying
/// byte transport (kernel TCP vs RDMA).
pub async fn run_tcp_client(opts: &ClientOpts) -> Result<(), Box<dyn std::error::Error>> {
    let uri = format!("https://{}:{}", opts.connect.ip(), opts.connect.port());
    let ssl_connector = tls_common::build_connector(&opts.cert);

    // Create connections over plain TCP (retry for parity; TCP rarely needs it).
    let mut clients = Vec::with_capacity(opts.connections);
    for i in 0..opts.connections {
        let channel = connect_with_retry(&format!("tcp conn {i}"), Duration::from_secs(15), || {
            let ssl_connector = ssl_connector.clone();
            let uri = uri.clone();
            async move {
                let endpoint = Endpoint::from_shared(uri)?;
                let transport = tonic_tls::TcpTransport::from_endpoint(&endpoint);
                let connector = tonic_tls::openssl::TlsConnector::new(
                    transport,
                    ssl_connector,
                    "rdma-bench".to_string(),
                );
                endpoint
                    .connect_with_connector(connector)
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
            }
        })
        .await?;
        clients.push(GreeterClient::new(channel));
    }

    eprintln!(
        "Connected {} clients to {} (mode=tcp, threads={})",
        opts.connections, uri, opts.threads
    );

    run_channel_clients(opts, clients, "tcp", "tcp").await
}

/// Shared warmup + benchmark + report loop for any tonic `Channel`-based client
/// (RDMA-tls or TCP). The connection setup differs; everything after is common.
async fn run_channel_clients(
    opts: &ClientOpts,
    clients: Vec<GreeterClient<tonic::transport::Channel>>,
    mode: &str,
    transport: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let payload = "x".repeat(opts.payload);

    // Concurrent in-flight RPCs per connection. Each is an independent request
    // loop sharing the connection's `Channel` (cheaply cloneable); h2 multiplexes
    // them as concurrent streams over the one RDMA connection, so a depth > 1
    // keeps several requests — and therefore several transport sends — pipelined
    // instead of the strict one-at-a-time ping-pong.
    //
    // send-recv and credit-ring pipeline safely at depth > 1.
    //
    // read-ring also pipelines at depth > 1 now. Its former `in_flight = 1` cap
    // guarded a *bidirectional* flow-control deadlock (both peers write-block
    // with spent doorbells, neither reads, so their Write+Imm RNR-stall with no
    // wakeup source). That is fixed by the doorbell-blocked RDMA-Read *heartbeat*
    // in `read_ring_transport::send_gather`: a doorbell-blocked sender posts a
    // one-sided Read whose completion re-polls the write path and pumps the
    // stream's recv-drain / doorbell-repost cascade, so a symmetric write-block
    // self-unwinds instead of wedging. Validated cross-VM on MANA RoCEv2 with a
    // reboot-clean A/B (`conn=4 in_flight=8 payload=256`: 5/5 pass with the
    // heartbeat vs 3/3 warmup-wedge without) and a boundary sweep up to
    // `conn=16 in_flight=64 payload=1024` — 0 deadlocks.
    //
    // NOTE (perf, not correctness): read-ring at deep pipelines with larger
    // payloads (e.g. `conn=8 in_flight=32 payload=1024`) collapses to a
    // low-throughput byte-ring-saturation regime (RDMA-Read head-refresh RTT
    // bubbles); prefer send-recv/credit-ring for large-payload pipelines.
    // See docs/bugs/read-ring-concurrent-stream-deadlock.md.
    let depth = opts.in_flight.max(1);

    // Client CPU + peak-RSS sampling window is [warmup_deadline, bench_deadline],
    // so the reported CPU-per-op excludes connection setup and warmup (mirrors
    // `--mode echo`). Both deadlines are computed up front and shared with the
    // warmup/benchmark loops below.
    let warmup_deadline = Instant::now() + Duration::from_secs(opts.warmup);
    let bench_deadline = warmup_deadline + Duration::from_secs(opts.duration);
    let sampler = spawn_resource_sampler(warmup_deadline, bench_deadline);

    // Warmup
    if opts.warmup > 0 {
        eprintln!("Warming up for {}s...", opts.warmup);
        let mut warmup_handles = Vec::new();
        for client in clients.clone() {
            for _ in 0..depth {
                let mut client = client.clone();
                let p = payload.clone();
                let deadline = warmup_deadline;
                warmup_handles.push(tokio::spawn(async move {
                    while Instant::now() < deadline {
                        let _ = client.say_hello(HelloRequest { name: p.clone() }).await;
                    }
                }));
            }
        }
        for h in warmup_handles {
            let _ = h.await;
        }
        eprintln!("Warmup complete");
    }

    // Benchmark. One histogram slot per concurrent request loop so the loops
    // never contend on the same lock.
    let metrics = BenchMetrics::new(opts.connections * depth);

    eprintln!(
        "Benchmarking for {}s ({depth} concurrent RPC(s) per connection)...",
        opts.duration
    );

    let mut handles = Vec::new();
    for (conn_id, client) in clients.into_iter().enumerate() {
        for sub in 0..depth {
            let slot = conn_id * depth + sub;
            let mut client = client.clone();
            let m = metrics.clone();
            let p = payload.clone();
            let deadline = bench_deadline;
            handles.push(tokio::spawn(async move {
                while Instant::now() < deadline {
                    let start = Instant::now();
                    match client.say_hello(HelloRequest { name: p.clone() }).await {
                        Ok(_) => {
                            m.record(slot, start.elapsed()).await;
                        }
                        Err(e) => {
                            tracing::trace!(conn_id, error = %e, "RPC failed");
                            m.record_error();
                        }
                    }
                }
            }));
        }
    }

    for (i, h) in handles.into_iter().enumerate() {
        if let Err(e) = h.await {
            tracing::warn!(task = i, error = %e, "bench task failed");
        }
    }

    // Report
    let (cpu_seconds, peak_rss_kb) = sampler.await.unwrap_or((0.0, 0));
    let hist = metrics.merged_histogram().await;
    let result = BenchResult::from_histogram(
        &hist,
        mode,
        transport,
        opts.connections,
        opts.threads,
        depth,
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

/// RDMA HTTP/3 gRPC client (`--mode rh3`): tonic-h3 over QUIC on an RDMA UDP
/// socket. HTTP/3 request/response runs one request outstanding per connection.
pub async fn run_h3_client<B>(
    builder: B,
    opts: &ClientOpts,
    transport_label: &str,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let payload = "x".repeat(opts.payload);
    let (certs, _key) = h3_common::load_certs_from_pem(&opts.cert, &opts.key);
    let client_config = h3_common::make_client_config(&certs);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    // H3 needs a bind address for the client socket
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

    // Create connections
    type H3Client = GreeterClient<tonic_h3::H3Channel<tonic_h3::quinn::H3QuinnConnector>>;
    let mut clients: Vec<H3Client> = Vec::with_capacity(opts.connections);

    // Each connection is established sequentially: bind an RdmaUdpSocket, then
    // `connect_to` (the full RDMA-CM handshake + ring token exchange) before
    // building the Quinn endpoint. On the Azure MANA RoCEv2 preview the rh3 RING
    // transports are slow to establish at high connection counts (~1s/conn,
    // ~60s for 64) — every combo still completes and works, but the benchmark
    // harness must allow setup time that scales with `connections` (see the
    // run-bench timeout in tests/e2e/playbooks/bench_run.yml). send-recv and the
    // rh2 transports are much faster.
    for i in 0..opts.connections {
        let client = connect_with_retry(&format!("rh3 conn {i}"), Duration::from_secs(30), || {
            let builder = builder.clone();
            let runtime = runtime.clone();
            let client_config = client_config.clone();
            let connect = opts.connect;
            let transport_name = transport_label.to_string();
            async move {
                let client_socket = RdmaUdpSocket::bind(&bind_addr, builder)
                    .map_err(|e| transport_setup_error(&transport_name, e))?;
                client_socket
                    .connect_to(&connect)
                    .await
                    .map_err(|e| transport_setup_error(&transport_name, e))?;

                let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
                    quinn::EndpointConfig::default(),
                    None,
                    Arc::new(client_socket),
                    runtime,
                )?;
                endpoint.set_default_client_config(client_config);

                let uri: http::Uri = format!("https://{}", connect).parse()?;
                let connector = tonic_h3::quinn::H3QuinnConnector::new(
                    uri.clone(),
                    "localhost".to_string(),
                    endpoint,
                );
                let channel = tonic_h3::H3Channel::new(connector, uri);
                Ok(GreeterClient::new(channel))
            }
        })
        .await?;
        clients.push(client);
    }

    eprintln!(
        "Connected {} H3 clients to {} (mode=rh3, threads={})",
        opts.connections, opts.connect, opts.threads
    );

    // Benchmark (includes warmup period — stats start after warmup)
    let warmup_duration = Duration::from_secs(opts.warmup);
    let bench_duration = Duration::from_secs(opts.duration);
    let total_deadline = Instant::now() + warmup_duration + bench_duration;
    let bench_start = Instant::now() + warmup_duration;

    eprintln!(
        "Warming up for {}s, then benchmarking for {}s...",
        opts.warmup, opts.duration
    );

    let metrics = BenchMetrics::new(opts.connections);

    let mut handles = Vec::new();
    for (conn_id, mut client) in clients.into_iter().enumerate() {
        let m = metrics.clone();
        let p = payload.clone();
        let deadline = total_deadline;
        let record_after = bench_start;
        handles.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                let start = Instant::now();
                match client.say_hello(HelloRequest { name: p.clone() }).await {
                    Ok(_) => {
                        if start >= record_after {
                            m.record(conn_id, start.elapsed()).await;
                        }
                    }
                    Err(e) => {
                        tracing::trace!(conn_id, error = %e, "H3 RPC failed");
                        if start >= record_after {
                            m.record_error();
                        }
                    }
                }
            }
        }));
    }

    for (i, h) in handles.into_iter().enumerate() {
        if let Err(e) = h.await {
            tracing::warn!(conn_id = i, error = %e, "H3 bench task failed");
        }
    }

    // Report
    let hist = metrics.merged_histogram().await;
    let result = BenchResult::from_histogram(
        &hist,
        "rh3",
        transport_label,
        opts.connections,
        opts.threads,
        1, // request/response: one request outstanding per connection
        opts.duration,
        opts.payload,
        metrics.total_errors.load(Ordering::Relaxed),
    );

    match opts.report.as_str() {
        "json" => result.print_json(),
        _ => result.print_text(),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

struct BenchGreeter;

#[tonic::async_trait]
impl Greeter for BenchGreeter {
    async fn say_hello(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<HelloReply>, Status> {
        let name = request.into_inner().name;
        Ok(Response::new(HelloReply {
            message: format!("Hello {name}"),
        }))
    }
}

/// RDMA HTTP/2 gRPC server (`--mode rh2`).
pub async fn run_rh2_server<B>(
    builder: B,
    opts: &ServerOpts,
    transport_label: &str,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let acceptor = tls_common::build_acceptor(&opts.cert, &opts.key);

    let listener = AsyncCmListener::bind(&opts.bind)?;
    let local_addr = listener.local_addr();
    let incoming = RdmaIncoming::new(listener, builder);
    let tls_incoming = tonic_tls::openssl::TlsIncoming::new(incoming, acceptor);

    eprintln!(
        "Benchmark server listening on {:?} (mode=rh2, transport={})",
        local_addr, transport_label
    );

    Server::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .serve_with_incoming_shutdown(tls_incoming, shutdown)
        .await?;

    Ok(())
}

/// TCP baseline gRPC server (`--mode tcp`): same TLS/gRPC stack as `rh2` over a
/// kernel TCP listener so the only variable is the underlying byte transport.
pub async fn run_tcp_server(
    opts: &ServerOpts,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let acceptor = tls_common::build_acceptor(&opts.cert, &opts.key);

    let tcp_incoming = tonic::transport::server::TcpIncoming::bind(opts.bind)?;
    let tls_incoming = tonic_tls::openssl::TlsIncoming::new(tcp_incoming, acceptor);

    eprintln!("Benchmark server listening on {} (mode=tcp)", opts.bind);

    Server::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .serve_with_incoming_shutdown(tls_incoming, shutdown)
        .await?;

    Ok(())
}

/// RDMA HTTP/3 gRPC server (`--mode rh3`): tonic-h3 over QUIC on an RDMA UDP
/// socket.
pub async fn run_h3_server<B>(
    builder: B,
    opts: &ServerOpts,
    transport_label: &str,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let (certs, key) = h3_common::load_certs_from_pem(&opts.cert, &opts.key);
    let server_config = h3_common::make_server_config(&certs, key);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let socket = Arc::new(RdmaUdpSocket::bind(&opts.bind, builder).expect("bind RDMA UDP socket"));
    let bound_addr = socket.bound_addr();

    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        runtime,
    )?;

    eprintln!(
        "Benchmark server listening on {} (mode=rh3, transport={})",
        bound_addr, transport_label
    );

    let acceptor = tonic_h3::quinn::H3QuinnAcceptor::new(endpoint.clone());
    let routes = tonic::service::Routes::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .clone()
        .routes();
    tonic_h3::server::H3Router::new(routes)
        .serve_with_shutdown(acceptor, shutdown)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;

    // Close the QUIC endpoint and wait for connections to drain so the RDMA
    // queue pairs are released cleanly before the process exits.
    endpoint.close(0u16.into(), b"shutdown");
    endpoint.wait_idle().await;

    Ok(())
}
