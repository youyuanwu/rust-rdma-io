use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport_common::MemoryWindowMode;
use rdma_io::wr::QpType;

use rdma_io_bench::common::ServerOpts;
use rdma_io_bench::{echo, grpc, h1};

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

    /// Number of connections to accept in `--mode echo-busy` (must match the
    /// client's --connections). The busy-poll server accepts exactly this many
    /// read-ring connections, sharded across the pinned cores, then echoes each
    /// on its owning core until the peers disconnect or a shutdown signal
    /// arrives. Ignored by the arm-park modes (which accept unbounded
    /// connections until shutdown).
    #[arg(long, default_value_t = 2)]
    connections: usize,

    /// Executor budget — how many CPUs the server uses. In the arm-park modes
    /// it is the number of tokio multi-thread worker threads (`0` = tokio
    /// default, one per core — the historical behaviour). In `--mode echo-busy`
    /// it is the number of **pinned busy-poll cores** in the pool (each spins
    /// one `CoreDriver` over its shared CQs; connections are sharded round-robin
    /// and are core-affine for life). Match the client's `--threads` for a fair
    /// CPU comparison.
    #[arg(long, default_value_t = 0)]
    threads: usize,

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

impl Args {
    /// Build the shared server run parameters passed to the per-protocol
    /// runners (`grpc`, `h1`).
    fn server_opts(&self) -> ServerOpts {
        ServerOpts {
            bind: self.bind,
            cert: self.cert.clone(),
            key: self.key.clone(),
        }
    }
}

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

/// Map the `--require-mw` flag to a [`MemoryWindowMode`]. Default is `Auto`
/// (detect + fall back to the MR rkey); `--require-mw` forces Memory Windows.
fn mw_mode(require_mw: bool) -> MemoryWindowMode {
    if require_mw {
        MemoryWindowMode::Require
    } else {
        MemoryWindowMode::Auto
    }
}

/// Resolve the ring transport's `max_in_flight` (send/doorbell/CQ depth). An
/// explicit `--ring-queue-depth` (>0) wins; otherwise size the ring from
/// `--in-flight` with headroom so the ring always has at least as many
/// outstanding-message slots as the peer pipelines, preventing the RNR
/// over-subscription collapse. The transport clamps to device queue limits.
/// Must derive identically to the client (same `--in-flight`).
fn ring_max_in_flight(ring_queue_depth: usize, in_flight: usize) -> Option<usize> {
    if ring_queue_depth > 0 {
        Some(ring_queue_depth)
    } else {
        Some((in_flight.max(1) * 2).max(16))
    }
}

/// Read-ring stream config for the gRPC/HTTP server modes.
fn ring_config_read(args: &Args) -> ReadRingConfig {
    let mut config = ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
    config.max_message_size = args.ring_max_msg;
    config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
    config
}

/// Credit-ring stream config for the gRPC/HTTP server modes.
fn ring_config_credit(args: &Args) -> CreditRingConfig {
    let mut config = CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
    config.max_message_size = args.ring_max_msg;
    config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
    config
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    // Busy-poll (`--mode echo-busy`) runs its data path on the BusyPool's pinned
    // `current_thread` runtimes, so `main` uses only a small single-threaded
    // orchestration runtime; the arm-park modes use the multi-thread runtime.
    if args.mode == "echo-busy" {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return rt.block_on(run_echo_busy_server(&args));
    }

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    // `--threads 0` = tokio's default (one worker per core, the historical
    // server behaviour); otherwise cap the worker pool.
    if args.threads > 0 {
        builder.worker_threads(args.threads);
    }
    let rt = builder.build()?;
    rt.block_on(run_arm_park(args))
}

/// Dispatch the arm-park (interrupt-driven) server modes on the multi-thread
/// runtime.
async fn run_arm_park(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let opts = args.server_opts();

    match args.mode.as_str() {
        "rh2" => dispatch_rh2_server(&args, &opts).await,
        "rh3" => dispatch_rh3_server(&args, &opts).await,
        "tcp" => grpc::run_tcp_server(&opts, shutdown_signal()).await,
        "rh1" => dispatch_rh1_server(&args, &opts).await,
        "tcp1" => h1::run_tcp1_server(&opts, shutdown_signal()).await,
        "echo" => run_echo_server(args).await,
        other => {
            eprintln!(
                "Unknown mode: {other}. Supported: rh2, rh3, tcp, rh1, tcp1, echo, echo-busy"
            );
            std::process::exit(1);
        }
    }
}

/// Busy-poll read-ring echo server (`--mode echo-busy`). The counterpart to the
/// client's `run_echo_busy_client`: accepts exactly `--connections` read-ring
/// connections across `--threads` pinned cores and echoes each on its owning
/// core. Only the read-ring transport is supported.
async fn run_echo_busy_server(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.transport != "read-ring" && args.transport != "send-recv" {
        eprintln!(
            "note: --mode echo-busy uses the read-ring transport; ignoring --transport {}",
            args.transport
        );
    }
    let mut config = ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
    config.max_message_size = args.ring_max_msg;
    config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
    echo::run_read_ring_busy_server(
        args.bind,
        config,
        args.threads,
        args.connections,
        shutdown_signal(),
    )
    .await
}

/// Build the selected RDMA transport for `rh2` and serve gRPC.
async fn dispatch_rh2_server(
    args: &Args,
    opts: &ServerOpts,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => {
            grpc::run_rh2_server(
                SendRecvConfig::stream_with_depth(args.in_flight),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        "read-ring" => {
            grpc::run_rh2_server(
                ring_config_read(args),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        "credit-ring" => {
            grpc::run_rh2_server(
                ring_config_credit(args),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

/// Build the selected RDMA transport for `rh1` and serve HTTP/1.1.
async fn dispatch_rh1_server(
    args: &Args,
    opts: &ServerOpts,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => {
            let depth = args.in_flight.max(1);
            h1::run_h1_server(
                SendRecvConfig::stream_with_depth(depth),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        "read-ring" => {
            h1::run_h1_server(
                ring_config_read(args),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        "credit-ring" => {
            h1::run_h1_server(
                ring_config_credit(args),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

/// Build the selected RDMA transport for `rh3` and serve HTTP/3.
async fn dispatch_rh3_server(
    args: &Args,
    opts: &ServerOpts,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => {
            grpc::run_h3_server(
                SendRecvConfig::datagram(),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        "read-ring" => {
            grpc::run_h3_server(
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        "credit-ring" => {
            grpc::run_h3_server(
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
                opts,
                &args.transport,
                shutdown_signal(),
            )
            .await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
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
            let config = ring_config_read(&args);
            echo::run_transport_echo_server(config, args.bind, "read-ring", shutdown_signal()).await
        }
        "credit-ring" => {
            let config = ring_config_credit(&args);
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
