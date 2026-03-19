use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io_quinn::RdmaUdpSocket;
use rdma_io_tonic::RdmaIncoming;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use rdma_io_bench::greeter::greeter_server::{Greeter, GreeterServer};
use rdma_io_bench::greeter::{HelloReply, HelloRequest};
use rdma_io_bench::{h3_common, tls_common};

#[derive(Parser)]
#[command(name = "rdma-bench-server", about = "RDMA gRPC benchmark server")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:50051")]
    bind: SocketAddr,

    #[arg(long, default_value = "tls")]
    mode: String,

    #[arg(long, default_value = "build/certs/cert.pem")]
    cert: PathBuf,

    #[arg(long, default_value = "build/certs/key.pem")]
    key: PathBuf,
}

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    match args.mode.as_str() {
        "tls" => run_tls_server(args).await,
        "h3" => run_h3_server(args).await,
        other => {
            eprintln!("Unknown mode: {other}. Supported: tls, h3");
            std::process::exit(1);
        }
    }
}

async fn run_tls_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let acceptor = tls_common::build_acceptor(&args.cert, &args.key);

    let listener = AsyncCmListener::bind(&args.bind)?;
    let local_addr = listener.local_addr();
    let incoming = RdmaIncoming::new(listener, SendRecvConfig::stream());
    let tls_incoming = tonic_tls::openssl::TlsIncoming::new(incoming, acceptor);

    eprintln!("Benchmark server listening on {:?} (mode=tls)", local_addr);

    Server::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .serve_with_incoming(tls_incoming)
        .await?;

    Ok(())
}

async fn run_h3_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let (certs, key) = h3_common::load_certs_from_pem(&args.cert, &args.key);
    let server_config = h3_common::make_server_config(&certs, key);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let socket = Arc::new(
        RdmaUdpSocket::bind(&args.bind, SendRecvConfig::datagram()).expect("bind RDMA UDP socket"),
    );
    let bound_addr = socket.bound_addr();

    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        runtime,
    )?;

    eprintln!("Benchmark server listening on {} (mode=h3)", bound_addr);

    let acceptor = tonic_h3::quinn::H3QuinnAcceptor::new(endpoint.clone());
    let routes = tonic::service::Routes::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .clone()
        .routes();
    tonic_h3::server::H3Router::new(routes)
        .serve(acceptor)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;

    Ok(())
}
