use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport::TransportBuilder;
use rdma_io::transport_common::MemoryWindowMode;
use rdma_io::wr::QpType;
use rdma_io_quinn::RdmaUdpSocket;
use rdma_io_tonic::RdmaIncoming;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use rdma_io_bench::greeter::greeter_server::{Greeter, GreeterServer};
use rdma_io_bench::greeter::{HelloReply, HelloRequest};
use rdma_io_bench::{echo, h3_common, tls_common};

#[derive(Parser)]
#[command(name = "rdma-bench-server", about = "RDMA gRPC benchmark server")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:50051")]
    bind: SocketAddr,

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

    /// Per-connection send/recv pipeline depth. In the gRPC modes this sizes
    /// the send-recv stream's QP buffers so up to this many sends can be
    /// outstanding. In echo mode with a ring transport it drives the
    /// auto-derived ring in-flight budget (see --ring-queue-depth). Either way
    /// it must match the client's `--in-flight` so both peers size symmetrically.
    #[arg(long, default_value_t = 1)]
    in_flight: usize,

    /// Ring transport `max_message_size` in bytes (ring transports; `echo` and
    /// `rh2`): the largest payload carried in a single RDMA message. In `echo`
    /// mode a larger payload is truncated to this; in `rh2` (gRPC byte stream) a
    /// larger write is fragmented into `ceil(len / max_message_size)` messages.
    /// This no longer bounds the in-flight budget — that is auto-derived from
    /// --in-flight (see --ring-queue-depth). Must match the client.
    #[arg(long, default_value_t = 1500)]
    ring_max_msg: usize,

    /// Advanced override for the ring transport's in-flight budget (ring
    /// transports; `echo` and `rh2`): sizes the send/doorbell/CQ queues for this
    /// many outstanding
    /// messages. 0 (default) auto-derives the budget from --in-flight with
    /// headroom so the ring always has at least as many slots as the peer keeps
    /// in flight, preventing the RNR over-subscription collapse. Set >0 only to
    /// force a specific depth. Must match the client. Also accepted as
    /// --ring-max-inflight.
    #[arg(long, visible_alias = "ring-max-inflight", default_value_t = 0)]
    ring_queue_depth: usize,

    #[arg(long, default_value = "build/certs/cert.pem")]
    cert: PathBuf,

    #[arg(long, default_value = "build/certs/key.pem")]
    key: PathBuf,
}

struct BenchGreeter;

/// Resolve when the process receives SIGTERM (default `kill`) or SIGINT
/// (Ctrl-C). Used to drive graceful server shutdown so RDMA queue pairs are
/// torn down cleanly (via `rdma_disconnect`) instead of being abandoned on an
/// abrupt process kill — abrupt teardown wedges the Azure MANA connection
/// manager and makes back-to-back benchmark runs fail.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    eprintln!("shutdown signal received; draining connections");
}

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
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    match args.mode.as_str() {
        "rh2" => run_tls_server(args).await,
        "rh3" => run_h3_server(args).await,
        "tcp" => run_tcp_server(args).await,
        "echo" => run_echo_server(args).await,
        other => {
            eprintln!("Unknown mode: {other}. Supported: rh2, rh3, tcp, echo");
            std::process::exit(1);
        }
    }
}

/// Direct transport-level echo server (no tonic/TLS/gRPC). `--transport tcp`
/// serves a raw-TCP byte echo; the others serve the raw RDMA transport.
async fn run_echo_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "tcp" => echo::run_tcp_echo_server(args.bind, shutdown_signal()).await,
        "send-recv" => {
            let config = SendRecvConfig {
                buf_size: 64 * 1024,
                num_recv_bufs: 64,
                num_send_bufs: 64,
                max_inline_data: 0,
                qp_type: QpType::Rc,
            };
            echo::run_transport_echo_server(config, args.bind, "send-recv", shutdown_signal()).await
        }
        "read-ring" => {
            let mut config =
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
            config.max_message_size = args.ring_max_msg;
            config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
            echo::run_transport_echo_server(config, args.bind, "read-ring", shutdown_signal()).await
        }
        "credit-ring" => {
            let mut config =
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
            config.max_message_size = args.ring_max_msg;
            config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
            echo::run_transport_echo_server(config, args.bind, "credit-ring", shutdown_signal())
                .await
        }
        other => {
            eprintln!(
                "Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring, tcp"
            );
            std::process::exit(1);
        }
    }
}

/// TCP baseline: tonic-tls + OpenSSL over a standard TCP socket (no RDMA).
///
/// Same TLS/gRPC stack as `tls` mode, served over a kernel TCP listener so the
/// only variable is the underlying byte transport.
async fn run_tcp_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let acceptor = tls_common::build_acceptor(&args.cert, &args.key);

    let tcp_incoming = tonic::transport::server::TcpIncoming::bind(args.bind)?;
    let tls_incoming = tonic_tls::openssl::TlsIncoming::new(tcp_incoming, acceptor);

    eprintln!("Benchmark server listening on {} (mode=tcp)", args.bind);

    Server::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .serve_with_incoming_shutdown(tls_incoming, shutdown_signal())
        .await?;

    Ok(())
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
/// ring from `--in-flight` with headroom so the ring always has at least as
/// many outstanding-message slots as the peer pipelines, preventing the RNR
/// over-subscription collapse. The transport clamps to device queue limits.
/// Must derive identically to the client (same `--in-flight`).
fn ring_max_in_flight(ring_queue_depth: usize, in_flight: usize) -> Option<usize> {
    if ring_queue_depth > 0 {
        Some(ring_queue_depth)
    } else {
        Some((in_flight.max(1) * 2).max(16))
    }
}

async fn run_tls_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let require_mw = args.require_mw;
    match args.transport.as_str() {
        "send-recv" => {
            let depth = args.in_flight;
            run_tls_server_with(args, SendRecvConfig::stream_with_depth(depth)).await
        }
        "read-ring" => {
            let mut config =
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(require_mw));
            config.max_message_size = args.ring_max_msg;
            config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
            run_tls_server_with(args, config).await
        }
        "credit-ring" => {
            let mut config =
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(require_mw));
            config.max_message_size = args.ring_max_msg;
            config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
            run_tls_server_with(args, config).await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

async fn run_tls_server_with<B>(args: Args, builder: B) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let acceptor = tls_common::build_acceptor(&args.cert, &args.key);

    let listener = AsyncCmListener::bind(&args.bind)?;
    let local_addr = listener.local_addr();
    let incoming = RdmaIncoming::new(listener, builder);
    let tls_incoming = tonic_tls::openssl::TlsIncoming::new(incoming, acceptor);

    eprintln!(
        "Benchmark server listening on {:?} (mode=rh2, transport={})",
        local_addr, args.transport
    );

    Server::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .serve_with_incoming_shutdown(tls_incoming, shutdown_signal())
        .await?;

    Ok(())
}

async fn run_h3_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let require_mw = args.require_mw;
    match args.transport.as_str() {
        "send-recv" => run_h3_server_with(args, SendRecvConfig::datagram()).await,
        "read-ring" => {
            run_h3_server_with(
                args,
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(require_mw)),
            )
            .await
        }
        "credit-ring" => {
            run_h3_server_with(
                args,
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(require_mw)),
            )
            .await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

async fn run_h3_server_with<B>(args: Args, builder: B) -> Result<(), Box<dyn std::error::Error>>
where
    B: TransportBuilder,
{
    let (certs, key) = h3_common::load_certs_from_pem(&args.cert, &args.key);
    let server_config = h3_common::make_server_config(&certs, key);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let socket = Arc::new(RdmaUdpSocket::bind(&args.bind, builder).expect("bind RDMA UDP socket"));
    let bound_addr = socket.bound_addr();

    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        runtime,
    )?;

    eprintln!(
        "Benchmark server listening on {} (mode=rh3, transport={})",
        bound_addr, args.transport
    );

    let acceptor = tonic_h3::quinn::H3QuinnAcceptor::new(endpoint.clone());
    let routes = tonic::service::Routes::builder()
        .add_service(GreeterServer::new(BenchGreeter))
        .clone()
        .routes();
    tonic_h3::server::H3Router::new(routes)
        .serve_with_shutdown(acceptor, shutdown_signal())
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;

    // Close the QUIC endpoint and wait for connections to drain so the RDMA
    // queue pairs are released cleanly before the process exits.
    endpoint.close(0u16.into(), b"shutdown");
    endpoint.wait_idle().await;

    Ok(())
}
