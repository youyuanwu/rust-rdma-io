use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use clap::Parser;
use rdma_io::rdma_transport::TransportConfig;
use rdma_io_tonic::tls::RdmaTransport;
use tonic::transport::Endpoint;

use rdma_io_bench::greeter::HelloRequest;
use rdma_io_bench::greeter::greeter_client::GreeterClient;
use rdma_io_bench::metrics::BenchMetrics;
use rdma_io_bench::report::BenchResult;
use rdma_io_bench::tls_common;

#[derive(Parser)]
#[command(name = "rdma-bench-client", about = "RDMA gRPC benchmark client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:50051")]
    connect: SocketAddr,

    #[arg(long, default_value = "tls")]
    mode: String,

    #[arg(long, default_value_t = 1)]
    threads: usize,

    #[arg(long, default_value_t = 1)]
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
        other => {
            eprintln!("Unknown mode: {other}. Supported: tls");
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
                    Err(_) => {
                        m.record_error();
                    }
                }
            }
        }));
    }

    for h in handles {
        h.await?;
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
