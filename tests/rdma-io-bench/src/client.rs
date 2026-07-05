use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::Parser;
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport::TransportBuilder;
use rdma_io::transport_common::MemoryWindowMode;
use rdma_io::wr::QpType;
use rdma_io_quinn::RdmaUdpSocket;
use rdma_io_tonic::tls::RdmaTransport;
use tonic::transport::Endpoint;

use rdma_io_bench::greeter::HelloRequest;
use rdma_io_bench::greeter::greeter_client::GreeterClient;
use rdma_io_bench::metrics::BenchMetrics;
use rdma_io_bench::report::BenchResult;
use rdma_io_bench::{echo, h3_common, tls_common};

#[derive(Parser)]
#[command(name = "rdma-bench-client", about = "RDMA gRPC benchmark client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:50051")]
    connect: SocketAddr,

    #[arg(long, default_value = "rh2")]
    mode: String,

    /// RDMA transport: send-recv, read-ring, or credit-ring.
    /// Works with both rh2 and rh3 modes.
    #[arg(long, default_value = "send-recv")]
    transport: String,

    /// Force Type-2 Memory Windows for the ring transports instead of the
    /// default auto-detect. By default the transport binds a Memory Window
    /// when the NIC supports one and falls back to the MR rkey automatically on
    /// NICs that report max_mw=0 (e.g. Azure MANA). Pass this to fail fast
    /// instead (useful on MW-capable NICs / in CI).
    #[arg(long, default_value_t = false)]
    require_mw: bool,

    /// Number of tokio runtime worker threads (executor pool size). This is
    /// NOT a load dimension: the offered load is driven by --connections
    /// (each connection is one task) times --in-flight, so raising --threads
    /// only changes how many OS threads service the async tasks, not the
    /// request concurrency.
    #[arg(long, default_value_t = 2)]
    threads: usize,

    #[arg(long, default_value_t = 2)]
    connections: usize,

    /// Requests kept outstanding per connection. In `echo` mode this is the
    /// transport-level pipeline depth; in the gRPC modes (`rh2`/`rh3`/`tcp`) it
    /// is the number of concurrent in-flight RPCs issued per connection (h2/h3
    /// multiplex them over the one connection).
    #[arg(long, default_value_t = 1)]
    in_flight: usize,

    /// Ring transport `max_message_size` in bytes (echo mode only): the largest
    /// payload carried in a single RDMA message; larger echo payloads are
    /// truncated to this. This no longer bounds the in-flight budget — that is
    /// auto-derived from --in-flight (see --ring-queue-depth). Must match the
    /// server.
    #[arg(long, default_value_t = 1500)]
    ring_max_msg: usize,

    /// Advanced override for the ring transport's in-flight budget (echo mode
    /// only): sizes the send/doorbell/CQ queues for this many outstanding
    /// messages. 0 (default) auto-derives the budget from --in-flight with
    /// headroom, so the ring always has at least as many slots as the client
    /// keeps in flight — this prevents the silent RNR over-subscription
    /// collapse that happens when a deep --in-flight falls back to the small
    /// ring_capacity/ring_max_msg default. Set >0 only to force a specific
    /// depth (e.g. to reproduce that collapse). Must match the server. Also
    /// accepted as --ring-max-inflight.
    #[arg(long, visible_alias = "ring-max-inflight", default_value_t = 0)]
    ring_queue_depth: usize,

    /// Number of times to retry a failed connection setup (echo mode only)
    /// before giving up, with a short linear backoff. Absorbs the MANA RoCEv2
    /// preview's intermittent CM "Unreachable"/"Rejected" failures at high
    /// connection counts so one flaky connect doesn't abort the whole run.
    #[arg(long, default_value_t = 5)]
    connect_retries: u32,

    #[arg(long, default_value_t = 10)]
    duration: u64,

    #[arg(long, default_value_t = 5)]
    warmup: u64,

    #[arg(long, default_value_t = 64)]
    payload: usize,

    #[arg(long, default_value = "build/certs/cert.pem")]
    cert: PathBuf,

    #[arg(long, default_value = "build/certs/key.pem")]
    key: PathBuf,

    #[arg(long, default_value = "text")]
    report: String,
}

/// Map the `--require-mw` flag to a [`MemoryWindowMode`]. Default is `Auto`
/// (detect + fall back to the MR rkey); `--require-mw` forces Memory Windows.
fn mw_mode(require_mw: bool) -> MemoryWindowMode {
    if require_mw {
        MemoryWindowMode::Require
    } else {
        MemoryWindowMode::Auto
    }
}

/// Resolve the ring transport's `max_in_flight` (send/doorbell/CQ depth) for
/// echo mode. An explicit `--ring-queue-depth` (>0) wins; otherwise size the
/// ring from the application's `--in-flight` with headroom so the ring always
/// has at least as many outstanding-message slots as the client pipelines.
/// This prevents the silent RNR over-subscription collapse that occurs when a
/// deep `--in-flight` would otherwise fall back to the small
/// `ring_capacity / ring_max_msg` default (~43 slots). The transport clamps the
/// result to the device's queue limits. Both peers derive the same value from
/// the same `--in-flight`, preserving the required client/server symmetry.
fn ring_max_in_flight(ring_queue_depth: usize, in_flight: usize) -> Option<usize> {
    if ring_queue_depth > 0 {
        Some(ring_queue_depth)
    } else {
        Some((in_flight.max(1) * 2).max(16))
    }
}

/// Wrap an RDMA setup error with a hint when a ring transport hits EINVAL,
/// which on Azure NICs means Memory Windows are unsupported (`max_mw=0`).
fn transport_setup_error(transport: &str, err: rdma_io::Error) -> Box<dyn std::error::Error> {
    if transport != "send-recv" {
        format!(
            "transport '{transport}' failed during RDMA setup: {err}\n\
             hint: read-ring and credit-ring auto-detect Memory Window support and \
             fall back to the MR rkey on NICs that report max_mw=0 (e.g. Azure MANA). \
             If you passed --require-mw on such a NIC, drop it to allow the fallback."
        )
        .into()
    } else {
        format!("transport '{transport}' failed during RDMA setup: {err}").into()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.threads)
        .enable_all()
        .build()?;

    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let uri = format!("https://{}:{}", args.connect.ip(), args.connect.port());
    let payload = "x".repeat(args.payload);

    match args.mode.as_str() {
        "rh2" => run_tls_bench(&args, &uri, &payload).await,
        "rh3" => run_h3_bench(&args, &uri, &payload).await,
        "tcp" => run_tcp_bench(&args, &uri, &payload).await,
        "echo" => run_echo_bench(&args).await,
        other => {
            eprintln!("Unknown mode: {other}. Supported: rh2, rh3, tcp, echo");
            std::process::exit(1);
        }
    }
}

/// Direct transport-level echo benchmark (no tonic/TLS/gRPC). `--transport tcp`
/// selects the raw-TCP baseline; the others drive the raw RDMA transport.
async fn run_echo_bench(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "tcp" => {
            echo::run_tcp_echo_client(
                args.connect,
                args.connections,
                args.connect_retries,
                args.in_flight,
                args.payload,
                args.warmup,
                args.duration,
                args.threads,
                &args.report,
            )
            .await
        }
        "send-recv" => {
            let config = SendRecvConfig {
                buf_size: args.payload.max(64),
                num_recv_bufs: args.in_flight + 2,
                num_send_bufs: args.in_flight + 2,
                max_inline_data: 0,
                qp_type: QpType::Rc,
            };
            run_echo_bench_with(args, config, "send-recv").await
        }
        "read-ring" => {
            warn_ring_payload(args.payload, args.ring_max_msg);
            let mut config =
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
            config.max_message_size = args.ring_max_msg;
            config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
            run_echo_bench_with(args, config, "read-ring").await
        }
        "credit-ring" => {
            warn_ring_payload(args.payload, args.ring_max_msg);
            let mut config =
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
            config.max_message_size = args.ring_max_msg;
            config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
            run_echo_bench_with(args, config, "credit-ring").await
        }
        other => {
            eprintln!(
                "Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring, tcp"
            );
            std::process::exit(1);
        }
    }
}

/// The ring transports carry one message per RDMA-Write, capped at
/// `ring_max_msg` bytes, so larger echo payloads are truncated to one message.
fn warn_ring_payload(payload: usize, ring_max_msg: usize) {
    if payload > ring_max_msg {
        eprintln!(
            "warning: echo payload {payload} > ring max_message_size {ring_max_msg}; \
             the ring transports will truncate each message to {ring_max_msg} bytes"
        );
    }
}

async fn run_echo_bench_with<B>(
    args: &Args,
    builder: B,
    transport_label: &str,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    echo::run_transport_echo_client(
        builder,
        args.connect,
        args.connections,
        args.connect_retries,
        args.in_flight,
        args.payload,
        args.warmup,
        args.duration,
        args.threads,
        transport_label,
        &args.report,
    )
    .await
}

async fn run_tls_bench(
    args: &Args,
    uri: &str,
    payload: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => {
            run_tls_bench_with(
                args,
                uri,
                payload,
                SendRecvConfig::stream_with_depth(args.in_flight),
            )
            .await
        }
        "read-ring" => {
            run_tls_bench_with(
                args,
                uri,
                payload,
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
            )
            .await
        }
        "credit-ring" => {
            run_tls_bench_with(
                args,
                uri,
                payload,
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
            )
            .await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

/// Establish one connection with bounded retries and backoff.
///
/// read-ring's RDMA-CM is flaky under connect churn on the Azure MANA RoCEv2
/// preview: a connection can transiently fail the handshake ("expected
/// Established, got Unreachable") or hang mid-handshake. A single-attempt connect
/// would abort the entire benchmark whenever any one of the N connections trips,
/// masking data-path results as setup flakiness. Retry each connection up to
/// `MAX_ATTEMPTS` times with linear backoff; bound every attempt with
/// `per_attempt` so a hung handshake is retried rather than blocking forever.
async fn connect_with_retry<T, F, Fut>(
    what: &str,
    per_attempt: Duration,
    mut make: F,
) -> Result<T, Box<dyn std::error::Error>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Box<dyn std::error::Error>>>,
{
    const MAX_ATTEMPTS: u32 = 5;
    let mut last_err: Option<Box<dyn std::error::Error>> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match tokio::time::timeout(per_attempt, make()).await {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => {
                eprintln!("{what}: connect attempt {attempt}/{MAX_ATTEMPTS} failed: {e}");
                last_err = Some(e);
            }
            Err(_) => {
                eprintln!(
                    "{what}: connect attempt {attempt}/{MAX_ATTEMPTS} timed out after {per_attempt:?}"
                );
                last_err = Some(format!("connect timed out after {per_attempt:?}").into());
            }
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
        }
    }
    Err(last_err.unwrap_or_else(|| "connect failed".into()))
}

async fn run_tls_bench_with<B>(
    args: &Args,
    uri: &str,
    payload: &str,
    builder: B,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let ssl_connector = tls_common::build_connector(&args.cert);
    let transport = RdmaTransport::new(builder);

    // Create connections (retry: read-ring's RDMA-CM is flaky under connect
    // churn on MANA — see connect_with_retry).
    let mut clients = Vec::with_capacity(args.connections);
    for i in 0..args.connections {
        let channel = connect_with_retry(&format!("rh2 conn {i}"), Duration::from_secs(15), || {
            let transport = transport.clone();
            let ssl_connector = ssl_connector.clone();
            let uri = uri.to_string();
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
        args.connections, uri, args.transport, args.threads
    );

    run_channel_clients(args, payload, clients, "rh2", &args.transport).await
}

/// TCP baseline: tonic-tls + OpenSSL over a standard TCP socket (no RDMA).
///
/// Uses the same TLS/gRPC stack as the RDMA `tls` mode so the only variable is
/// the underlying byte transport (kernel TCP vs RDMA).
async fn run_tcp_bench(
    args: &Args,
    uri: &str,
    payload: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let ssl_connector = tls_common::build_connector(&args.cert);

    // Create connections over plain TCP (retry for parity; TCP rarely needs it).
    let mut clients = Vec::with_capacity(args.connections);
    for i in 0..args.connections {
        let channel = connect_with_retry(&format!("tcp conn {i}"), Duration::from_secs(15), || {
            let ssl_connector = ssl_connector.clone();
            let uri = uri.to_string();
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
        args.connections, uri, args.threads
    );

    run_channel_clients(args, payload, clients, "tcp", "tcp").await
}

/// Shared warmup + benchmark + report loop for any tonic `Channel`-based client
/// (RDMA-tls or TCP). The connection setup differs; everything after is common.
async fn run_channel_clients(
    args: &Args,
    payload: &str,
    clients: Vec<GreeterClient<tonic::transport::Channel>>,
    mode: &str,
    transport: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Concurrent in-flight RPCs per connection. Each is an independent request
    // loop sharing the connection's `Channel` (cheaply cloneable); h2 multiplexes
    // them as concurrent streams over the one RDMA connection, so a depth > 1
    // keeps several requests — and therefore several transport sends — pipelined
    // instead of the strict one-at-a-time ping-pong.
    //
    // send-recv and credit-ring pipeline safely at depth > 1.
    //
    // read-ring is capped to one in-flight RPC per connection. Two transport
    // lost-wakeup bugs were fixed along the way — the *unidirectional* send-CQ
    // reap (`send_copy` no longer polls the send CQ; `pipelined_transfer_read_ring`
    // is 20/20) and the write-blocked path not registering its recv waker (it now
    // drains `poll_recv` to `Pending`). Those make read-ring gRPC reliable at
    // modest aggregate load (validated: conn≤8 in_flight≤8, 64 B, 0 stalls,
    // ≈113k rps at conn=8 in_flight=4). But the *bidirectional* flow-control
    // deadlock that fix A only mitigates is still reachable at higher aggregate
    // load: `conn=4 in_flight=8 payload=256` deterministically wedges in warmup
    // on MANA RoCEv2 (both peers write-block with full stashes, neither reads).
    // Because the safe boundary depends on connections × depth × payload, the
    // benchmark keeps the conservative cap; use send-recv or credit-ring for
    // pipelined gRPC. See docs/bugs/read-ring-concurrent-stream-deadlock.md.
    let mut depth = args.in_flight.max(1);
    if transport == "read-ring" && depth > 1 {
        eprintln!(
            "warning: read-ring gRPC has a residual bidirectional stall at high \
             aggregate load; capping --in-flight from {} to 1 \
             (see docs/bugs/read-ring-concurrent-stream-deadlock.md)",
            args.in_flight
        );
        depth = 1;
    }

    // Warmup
    if args.warmup > 0 {
        eprintln!("Warming up for {}s...", args.warmup);
        let warmup_deadline = Instant::now() + Duration::from_secs(args.warmup);
        let mut warmup_handles = Vec::new();
        for client in clients.clone() {
            for _ in 0..depth {
                let mut client = client.clone();
                let p = payload.to_string();
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
    let metrics = BenchMetrics::new(args.connections * depth);
    let bench_deadline = Instant::now() + Duration::from_secs(args.duration);

    eprintln!(
        "Benchmarking for {}s ({depth} concurrent RPC(s) per connection)...",
        args.duration
    );

    let mut handles = Vec::new();
    for (conn_id, client) in clients.into_iter().enumerate() {
        for sub in 0..depth {
            let slot = conn_id * depth + sub;
            let mut client = client.clone();
            let m = metrics.clone();
            let p = payload.to_string();
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
    let hist = metrics.merged_histogram().await;
    let result = BenchResult::from_histogram(
        &hist,
        mode,
        transport,
        args.connections,
        args.threads,
        depth,
        args.duration,
        args.payload,
        metrics.total_errors.load(Ordering::Relaxed),
    );

    match args.report.as_str() {
        "json" => result.print_json(),
        _ => result.print_text(),
    }

    Ok(())
}

async fn run_h3_bench(
    args: &Args,
    _uri: &str,
    payload: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => run_h3_bench_with(args, payload, SendRecvConfig::datagram()).await,
        "read-ring" => {
            run_h3_bench_with(
                args,
                payload,
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
            )
            .await
        }
        "credit-ring" => {
            run_h3_bench_with(
                args,
                payload,
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
            )
            .await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

async fn run_h3_bench_with<B>(
    args: &Args,
    payload: &str,
    builder: B,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let (certs, _key) = h3_common::load_certs_from_pem(&args.cert, &args.key);
    let client_config = h3_common::make_client_config(&certs);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    // H3 needs a bind address for the client socket
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

    // Create connections
    type H3Client = GreeterClient<tonic_h3::H3Channel<tonic_h3::quinn::H3QuinnConnector>>;
    let mut clients: Vec<H3Client> = Vec::with_capacity(args.connections);

    // Each connection is established sequentially: bind an RdmaUdpSocket, then
    // `connect_to` (the full RDMA-CM handshake + ring token exchange) before
    // building the Quinn endpoint. On the Azure MANA RoCEv2 preview the rh3 RING
    // transports are slow to establish at high connection counts (~1s/conn,
    // ~60s for 64) — every combo still completes and works, but the benchmark
    // harness must allow setup time that scales with `connections` (see the
    // run-bench timeout in tests/e2e/playbooks/bench_run.yml). send-recv and the
    // rh2 transports are much faster.
    for i in 0..args.connections {
        let client = connect_with_retry(&format!("rh3 conn {i}"), Duration::from_secs(30), || {
            let builder = builder.clone();
            let runtime = runtime.clone();
            let client_config = client_config.clone();
            let connect = args.connect;
            let transport_name = args.transport.clone();
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
        args.connections, args.connect, args.threads
    );

    // Benchmark (includes warmup period — stats start after warmup)
    let warmup_duration = Duration::from_secs(args.warmup);
    let bench_duration = Duration::from_secs(args.duration);
    let total_deadline = Instant::now() + warmup_duration + bench_duration;
    let bench_start = Instant::now() + warmup_duration;

    eprintln!(
        "Warming up for {}s, then benchmarking for {}s...",
        args.warmup, args.duration
    );

    let metrics = BenchMetrics::new(args.connections);

    let mut handles = Vec::new();
    for (conn_id, mut client) in clients.into_iter().enumerate() {
        let m = metrics.clone();
        let p = payload.to_string();
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
        &args.transport,
        args.connections,
        args.threads,
        1, // request/response: one request outstanding per connection
        args.duration,
        args.payload,
        metrics.total_errors.load(Ordering::Relaxed),
    );

    match args.report.as_str() {
        "json" => result.print_json(),
        _ => result.print_text(),
    }

    Ok(())
}
