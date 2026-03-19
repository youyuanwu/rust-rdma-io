use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::Parser;
use rdma_io::rdma_transport::TransportConfig;
use rdma_io_quinn::RdmaUdpSocket;
use rdma_io_tonic::tls::RdmaTransport;
use tonic::transport::Endpoint;

use rdma_io_bench::greeter::HelloRequest;
use rdma_io_bench::greeter::greeter_client::GreeterClient;
use rdma_io_bench::metrics::BenchMetrics;
use rdma_io_bench::report::BenchResult;
use rdma_io_bench::{h3_common, tls_common};

#[derive(Parser)]
#[command(name = "rdma-bench-client", about = "RDMA gRPC benchmark client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:50051")]
    connect: SocketAddr,

    #[arg(long, default_value = "tls")]
    mode: String,

    #[arg(long, default_value_t = 2)]
    threads: usize,

    #[arg(long, default_value_t = 2)]
    connections: usize,

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(false)
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
        "tls" => run_tls_bench(&args, &uri, &payload).await,
        "h3" => run_h3_bench(&args, &uri, &payload).await,
        other => {
            eprintln!("Unknown mode: {other}. Supported: tls, h3");
            std::process::exit(1);
        }
    }
}

async fn run_tls_bench(
    args: &Args,
    uri: &str,
    payload: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let ssl_connector = tls_common::build_connector(&args.cert);
    let transport = RdmaTransport::new(TransportConfig::stream());

    // Create connections
    let mut clients = Vec::with_capacity(args.connections);
    for _ in 0..args.connections {
        let connector = tonic_tls::openssl::TlsConnector::new(
            transport.clone(),
            ssl_connector.clone(),
            "rdma-bench".to_string(),
        );
        let channel = Endpoint::from_shared(uri.to_string())?
            .connect_with_connector(connector)
            .await?;
        clients.push(GreeterClient::new(channel));
    }

    eprintln!(
        "Connected {} clients to {} (mode=tls, threads={})",
        args.connections, uri, args.threads
    );

    // Warmup
    if args.warmup > 0 {
        eprintln!("Warming up for {}s...", args.warmup);
        let warmup_deadline = Instant::now() + Duration::from_secs(args.warmup);
        let mut warmup_handles = Vec::new();
        for mut client in clients.clone() {
            let p = payload.to_string();
            let deadline = warmup_deadline;
            warmup_handles.push(tokio::spawn(async move {
                while Instant::now() < deadline {
                    let _ = client.say_hello(HelloRequest { name: p.clone() }).await;
                }
            }));
        }
        for h in warmup_handles {
            let _ = h.await;
        }
        eprintln!("Warmup complete");
    }

    // Benchmark
    let metrics = BenchMetrics::new(args.connections);
    let bench_deadline = Instant::now() + Duration::from_secs(args.duration);

    eprintln!("Benchmarking for {}s...", args.duration);

    let mut handles = Vec::new();
    for (conn_id, mut client) in clients.into_iter().enumerate() {
        let m = metrics.clone();
        let p = payload.to_string();
        let deadline = bench_deadline;
        handles.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                let start = Instant::now();
                match client.say_hello(HelloRequest { name: p.clone() }).await {
                    Ok(_) => {
                        m.record(conn_id, start.elapsed()).await;
                    }
                    Err(e) => {
                        tracing::trace!(conn_id, error = %e, "TLS RPC failed");
                        m.record_error();
                    }
                }
            }
        }));
    }

    for (i, h) in handles.into_iter().enumerate() {
        if let Err(e) = h.await {
            tracing::warn!(conn_id = i, error = %e, "TLS bench task failed");
        }
    }

    // Report
    let hist = metrics.merged_histogram().await;
    let result = BenchResult::from_histogram(
        &hist,
        "tls",
        "stream",
        args.connections,
        args.threads,
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
    let (certs, _key) = h3_common::load_certs_from_pem(&args.cert, &args.key);
    let client_config = h3_common::make_client_config(&certs);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    // H3 needs a bind address for the client socket
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

    // Create connections
    type H3Client = GreeterClient<tonic_h3::H3Channel<tonic_h3::quinn::H3QuinnConnector>>;
    let mut clients: Vec<H3Client> = Vec::with_capacity(args.connections);

    for _ in 0..args.connections {
        let client_socket = RdmaUdpSocket::bind(&bind_addr, TransportConfig::datagram())
            .expect("bind client socket");
        client_socket
            .connect_to(&args.connect)
            .await
            .expect("RDMA pre-connect");

        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            Arc::new(client_socket),
            runtime.clone(),
        )?;
        endpoint.set_default_client_config(client_config.clone());

        let uri: http::Uri = format!("https://{}", args.connect).parse()?;
        let connector =
            tonic_h3::quinn::H3QuinnConnector::new(uri.clone(), "localhost".to_string(), endpoint);
        let channel = tonic_h3::H3Channel::new(connector, uri);
        clients.push(GreeterClient::new(channel));
    }

    eprintln!(
        "Connected {} H3 clients to {} (mode=h3, threads={})",
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
        "h3",
        "datagram",
        args.connections,
        args.threads,
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
