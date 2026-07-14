use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport::TransportBuilder;
use rdma_io::transport_common::MemoryWindowMode;
use rdma_io::wr::QpType;

use rdma_io_bench::common::ClientOpts;
use rdma_io_bench::{echo, grpc, h1};

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

    /// Executor budget — how many CPUs this process uses. In the arm-park modes
    /// (`rh2`/`rh3`/`rh1`/`tcp`/`tcp1`/`echo`) it is the number of tokio
    /// multi-thread worker threads (`0` = tokio default, one per core); it is
    /// NOT a load dimension (offered load is `--connections` × `--in-flight`).
    /// In `--mode echo-busy` it is the number of **pinned busy-poll cores** in
    /// the pool (each runs one `CoreDriver` spinning `ibv_poll_cq`; connections
    /// are sharded round-robin and are core-affine for life). Both peers should
    /// use the same value for a fair CPU comparison.
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

    /// Ring transport `max_message_size` in bytes (ring transports; `echo` and
    /// `rh2`): the largest payload carried in a single RDMA message. In `echo`
    /// mode a larger payload is truncated to this; in `rh2` (gRPC byte stream) a
    /// larger write is fragmented into `ceil(len / max_message_size)` messages,
    /// so raising it avoids fragmenting large gRPC payloads (e.g. set 8192 for an
    /// 8 KiB payload to send one message instead of ~6). This no longer bounds
    /// the in-flight budget — that is auto-derived from --in-flight (see
    /// --ring-queue-depth). Must match the server.
    #[arg(long, default_value_t = 1500)]
    ring_max_msg: usize,

    /// Advanced override for the ring transport's in-flight budget (ring
    /// transports; `echo` and `rh2`): sizes the send/doorbell/CQ queues for this
    /// many outstanding
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

impl Args {
    /// Build the shared client run parameters passed to the per-protocol
    /// runners (`grpc`, `h1`).
    fn client_opts(&self) -> ClientOpts {
        ClientOpts {
            connect: self.connect,
            connections: self.connections,
            threads: self.threads,
            in_flight: self.in_flight,
            warmup: self.warmup,
            duration: self.duration,
            payload: self.payload,
            cert: self.cert.clone(),
            key: self.key.clone(),
            report: self.report.clone(),
        }
    }
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
/// explicit `--ring-queue-depth` (>0) wins; otherwise size the ring from the
/// application's `--in-flight` with headroom so the ring always has at least as
/// many outstanding-message slots as the client pipelines. This prevents the
/// silent RNR over-subscription collapse that occurs when a deep `--in-flight`
/// would otherwise fall back to the small `ring_capacity / ring_max_msg` default
/// (~43 slots). The transport clamps the result to the device's queue limits.
/// Both peers derive the same value from the same `--in-flight`, preserving the
/// required client/server symmetry.
fn ring_max_in_flight(ring_queue_depth: usize, in_flight: usize) -> Option<usize> {
    if ring_queue_depth > 0 {
        Some(ring_queue_depth)
    } else {
        Some((in_flight.max(1) * 2).max(16))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    // Busy-poll (`--mode echo-busy`) has its own runtime topology: the data path
    // runs on the BusyPool's N pinned `current_thread` runtimes, so `main` uses
    // only a small single-threaded orchestration runtime (probe + awaiting the
    // pool's per-connection handles + the resource sampler) rather than the
    // multi-thread worker pool the arm-park modes drive.
    if args.mode == "echo-busy" {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return rt.block_on(run_echo_busy_client(&args));
    }

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    // `--threads 0` = tokio's default (one worker per core); otherwise cap.
    if args.threads > 0 {
        builder.worker_threads(args.threads);
    }
    let rt = builder.build()?;

    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let opts = args.client_opts();

    match args.mode.as_str() {
        "rh2" => dispatch_rh2(&args, &opts).await,
        "rh3" => dispatch_rh3(&args, &opts).await,
        "tcp" => grpc::run_tcp_client(&opts).await,
        "rh1" => dispatch_rh1(&args, &opts).await,
        "tcp1" => h1::run_tcp1_client(&opts).await,
        "echo" => run_echo_bench(&args).await,
        other => {
            eprintln!(
                "Unknown mode: {other}. Supported: rh2, rh3, tcp, rh1, tcp1, echo, echo-busy"
            );
            std::process::exit(1);
        }
    }
}

/// Busy-poll read-ring echo client (`--mode echo-busy`). Builds the read-ring
/// config from the same ring knobs as `echo`, then drives it through the
/// thread-per-core [`BusyPool`](rdma_io_busy::BusyPool). Only the read-ring
/// transport is supported (busy-poll is a read-ring completion mode).
async fn run_echo_busy_client(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.transport != "read-ring" && args.transport != "send-recv" {
        eprintln!(
            "note: --mode echo-busy uses the read-ring transport; ignoring --transport {}",
            args.transport
        );
    }
    warn_ring_payload(args.payload, args.ring_max_msg);
    let mut config = ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
    config.max_message_size = args.ring_max_msg;
    config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
    echo::run_read_ring_busy_client(
        args.connect,
        config,
        args.threads,
        args.connections,
        args.in_flight,
        args.payload,
        args.warmup,
        args.duration,
        &args.report,
    )
    .await
}

/// Build the selected RDMA transport for `rh2` and drive the gRPC client.
async fn dispatch_rh2(args: &Args, opts: &ClientOpts) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => {
            grpc::run_rh2_client(
                SendRecvConfig::stream_with_depth(args.in_flight),
                opts,
                &args.transport,
            )
            .await
        }
        "read-ring" => grpc::run_rh2_client(ring_config_read(args), opts, &args.transport).await,
        "credit-ring" => {
            grpc::run_rh2_client(ring_config_credit(args), opts, &args.transport).await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

/// Build the selected RDMA transport for `rh1` and drive the HTTP/1.1 client.
async fn dispatch_rh1(args: &Args, opts: &ClientOpts) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => {
            h1::run_h1_client(
                SendRecvConfig::stream_with_depth(args.in_flight),
                opts,
                &args.transport,
            )
            .await
        }
        "read-ring" => h1::run_h1_client(ring_config_read(args), opts, &args.transport).await,
        "credit-ring" => h1::run_h1_client(ring_config_credit(args), opts, &args.transport).await,
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

/// Build the selected RDMA transport for `rh3` and drive the HTTP/3 client.
async fn dispatch_rh3(args: &Args, opts: &ClientOpts) -> Result<(), Box<dyn std::error::Error>> {
    match args.transport.as_str() {
        "send-recv" => grpc::run_h3_client(SendRecvConfig::datagram(), opts, &args.transport).await,
        "read-ring" => {
            grpc::run_h3_client(
                ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
                opts,
                &args.transport,
            )
            .await
        }
        "credit-ring" => {
            grpc::run_h3_client(
                CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw)),
                opts,
                &args.transport,
            )
            .await
        }
        other => {
            eprintln!("Unknown transport: {other}. Supported: send-recv, read-ring, credit-ring");
            std::process::exit(1);
        }
    }
}

/// Read-ring stream config for the gRPC/HTTP modes: sizes `max_message_size`
/// and the in-flight budget from `--ring-max-msg` / `--in-flight`.
fn ring_config_read(args: &Args) -> ReadRingConfig {
    let mut config = ReadRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
    config.max_message_size = args.ring_max_msg;
    config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
    config
}

/// Credit-ring stream config for the gRPC/HTTP modes (see [`ring_config_read`]).
fn ring_config_credit(args: &Args) -> CreditRingConfig {
    let mut config = CreditRingConfig::datagram().with_memory_window_mode(mw_mode(args.require_mw));
    config.max_message_size = args.ring_max_msg;
    config.max_in_flight = ring_max_in_flight(args.ring_queue_depth, args.in_flight);
    config
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
