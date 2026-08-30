# rdma-io

Safe Rust bindings for RDMA programming over [libibverbs](https://github.com/linux-rdma/rdma-core) and [librdmacm](https://github.com/linux-rdma/rdma-core), with async support, [tonic](https://github.com/hyperium/tonic) gRPC integration, and [Quinn](https://github.com/quinn-rs/quinn) QUIC integration.

## Features

- **Safe RAII wrappers** — `ProtectionDomain`, `CompletionQueue`, `MemoryRegion`, `QueuePair`, `CmId`, etc. with `Arc`-based ownership enforcing correct destruction order
- **Async stream** — `AsyncRdmaStream` implements `tokio::io::AsyncRead` + `AsyncWrite` and `futures::AsyncRead` + `AsyncWrite` with dual completion queues for full-duplex I/O
- **Transport trait** — Generic `Transport` / `TransportBuilder` abstraction decoupling RDMA mechanics from consumers; three concrete implementations: `SendRecvTransport` (Send/Recv verbs), `CreditRingTransport` (RDMA Write ring + credits), `ReadRingTransport` (RDMA Write ring + Read flow control)
- **Low-level async** — `AsyncCq` (completion queue polling via epoll) and `AsyncQp` for custom RDMA verb patterns (Send/Recv, RDMA Read/Write, atomics)
- **tonic gRPC transport** — `RdmaConnector` and `RdmaIncoming` for drop-in RDMA transport with tonic, including optional TLS via `tonic-tls` + OpenSSL
- **Quinn QUIC transport** — `RdmaUdpSocket` implements Quinn's `AsyncUdpSocket` trait, enabling QUIC over RDMA without modifying Quinn
- **tonic-h3 gRPC over HTTP/3** — Full stack: tonic gRPC → HTTP/3 → QUIC (Quinn) → RDMA, via `tonic-h3` integration
- **Generated FFI** — `rdma-io-sys` provides bindings generated with [bnd](https://github.com/youyuanwu/bnd), including wrappers for `ibverbs` inline functions

## Workspace Crates

| Crate | Description |
|---|---|
| `rdma-io` | Safe high-level API (async streams, Transport trait, connection management, QP verbs) |
| `rdma-io-tonic` | tonic gRPC transport over RDMA (connector, incoming, optional TLS) |
| `rdma-io-quinn` | Quinn QUIC over RDMA (`AsyncUdpSocket` implementation via Transport trait) |
| `rdma-io-sys` | Raw FFI bindings (`libibverbs` + `librdmacm`) |
| `bnd-rdma-gen` | Binding generator (dev-only) |
| `rdma-io-tests` | Integration tests (streams, QP verbs, tonic gRPC, TLS, Quinn QUIC, tonic-h3) |

## Quick Start

### Async Stream (tokio)

```rust
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::send_recv_transport::SendRecvConfig;

let config = SendRecvConfig::stream();

// Server
let listener = AsyncCmListener::bind(&"0.0.0.0:0".parse().unwrap()).unwrap();
let transport = config.accept(&listener).await.unwrap();
let mut server = AsyncRdmaStream::new(transport);

// Client
let addr = "10.0.0.1:9999".parse().unwrap();
let transport = config.connect(&addr).await.unwrap();
let mut client = AsyncRdmaStream::new(transport);
client.write_all(b"hello async rdma").await.unwrap();
```

### tonic gRPC over RDMA

```rust
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io_tonic::{RdmaConnector, RdmaIncoming};

// Server
let incoming = RdmaIncoming::bind(
    &"0.0.0.0:50051".parse().unwrap(),
    SendRecvConfig::stream(),
)?;
Server::builder()
    .add_service(my_service)
    .serve_with_incoming(incoming).await?;

// Client
let connector = RdmaConnector::new(SendRecvConfig::stream());
let channel = Endpoint::from_static("http://10.0.0.1:50051")
    .connect_with_connector(connector).await?;
```

### Quinn QUIC over RDMA

```rust
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io_quinn::RdmaUdpSocket;
use quinn::{Endpoint, EndpointConfig};

// Server: bind RDMA socket with transport builder, create Quinn endpoint
let server_socket = Arc::new(
    RdmaUdpSocket::bind(&"0.0.0.0:0".parse().unwrap(), SendRecvConfig::datagram())?
);
let server_endpoint = Endpoint::new_with_abstract_socket(
    EndpointConfig::default(), Some(server_config),
    server_socket, runtime,
)?;
let incoming = server_endpoint.accept().await.unwrap();

// Client: pre-connect RDMA, then create Quinn endpoint
let client_socket = RdmaUdpSocket::bind(&"0.0.0.0:0".parse().unwrap(), SendRecvConfig::datagram())?;
client_socket.connect_to(&server_addr).await?;
let client_endpoint = Endpoint::new_with_abstract_socket(
    EndpointConfig::default(), None,
    Arc::new(client_socket), runtime,
)?;
let connection = client_endpoint.connect(server_addr, "localhost")?.await?;
```

### tonic gRPC over HTTP/3 over RDMA

```rust
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io_quinn::RdmaUdpSocket;
use tonic_h3::quinn::{H3QuinnAcceptor, H3QuinnConnector};

// Server: RDMA socket → Quinn endpoint → tonic-h3 acceptor
let socket = Arc::new(RdmaUdpSocket::bind(&addr, SendRecvConfig::datagram())?);
let endpoint = Endpoint::new_with_abstract_socket(config, Some(h3_server_config), socket, rt)?;
let acceptor = H3QuinnAcceptor::new(endpoint);
tonic_h3::server::H3Router::new(routes).serve(acceptor).await?;

// Client: pre-connect RDMA → Quinn endpoint → H3 channel → gRPC client
let socket = RdmaUdpSocket::bind(&addr, SendRecvConfig::datagram())?;
socket.connect_to(&server_addr).await?;
let endpoint = Endpoint::new_with_abstract_socket(config, None, Arc::new(socket), rt)?;
let connector = H3QuinnConnector::new(uri.clone(), "localhost".into(), endpoint);
let channel = tonic_h3::H3Channel::new(connector, uri);
let client = GreeterClient::new(channel);
```

### V2 API — Per-Operation Futures (tokio)

The `v2` module provides ergonomic per-operation async futures with compio-style buffer ownership:

```rust
use rdma_io::v2::*;

// Resource setup via builders
let ctx = Context::from_cm(&cm_id)?;
let pd = ctx.alloc_pd()?;
let send_cq = CqBuilder::new(&ctx, 64).with_channel().build()?;
let recv_cq = CqBuilder::new(&ctx, 64).with_channel().build()?;
let qp = QpBuilder::new(&pd, &send_cq, &recv_cq).build_with_cm(&cm_id)?;

// Spawn completion driver
let (driver, handle) = FdCqDriver::new(send_cq, 64);
tokio::spawn(driver.run_tokio());

let sqp = SharedQp::new(qp, handle, pd);

// Per-operation futures: owned buffer in → (result, buffer) out
let mut mr = sqp.pd().reg_mr(1024, AccessIntent::LocalOnly)?;
mr.as_mut_slice()[..5].copy_from_slice(b"hello");
let (result, mr) = sqp.send(mr, None).await;
result?;

// One-sided RDMA Write
let (result, mr) = sqp.write(mr, remote_mr, None).await;
result?;
```

Also provides lower-level APIs: `Op` enum for typed submission, `CqPoller` for
direct CQ polling with waker registration, and `Completions<N>` for fd-based
async CQ draining. See the `rdma_io::v2` module docs for the full API reference.

### V2 Message Transport

The v2 message transport provides a builder-driven, message-oriented Send/Recv
transport on top of `SharedQp` with pre-registered buffer pools, message
boundaries, credit-based flow control, deterministic disconnect handling,
and cancellation-safe operations.

Construction returns a `(MessageTransport, MessageTransportDriver)` pair. The
caller explicitly spawns the driver future — exactly one task per endpoint:

```rust
use rdma_io::v2::*;
use rdma_io::v2::message_transport::MessageTransportBuilder;

// Server
let listener = rdma_io::async_cm::AsyncCmListener::bind(&"0.0.0.0:7471".parse().unwrap())?;
let (server, server_driver) = MessageTransportBuilder::new()
    .recv_buffers(32)
    .send_buffers(16)
    .buffer_size(64 * 1024)
    .completion_mode(CompletionMode::Readiness)
    .accept(&listener)
    .await?;
let server_task = tokio::spawn(server_driver);

// Client
let (client, client_driver) = MessageTransportBuilder::new()
    .recv_buffers(32)
    .send_buffers(16)
    .buffer_size(64 * 1024)
    .connect("10.0.0.1:7471".parse().unwrap())
    .await?;
let client_task = tokio::spawn(client_driver);

// Wait for readiness (HELLO handshake), then send/recv
client.ready().await?;
client.send(b"hello rdma transport").await?;
let msg = server.recv().await?;
assert_eq!(msg.as_ref(), b"hello rdma transport");

// Shutdown: close frontend, then await driver
client.close().await;
let driver_result = client_task.await.expect("driver panicked");
driver_result?;
```

Key design properties:

- **Explicit driver spawning**: No hidden `tokio::spawn` — the driver future
  composes CQ driving, receive pumping, HELLO/credit protocol, CM disconnect
  monitoring, and cancellation reclamation into one `Future + Send + 'static`.
  One user-spawned task per endpoint in both shared and separate CQ modes.
- **Wire protocol**: A minimal internal protocol with DATA, CREDIT, and HELLO
  frame types (12-byte header with magic/version/type/length validation)
- **Credit-based flow control**: Each `send()` acquires one remote receive
  credit. Credits are exchanged via HELLO handshake and returned via CREDIT
  frames when `ReceivedMessage` is dropped. RNR retry is a safety net, not
  the primary flow-control mechanism.
- **Readiness handshake**: The driver performs the HELLO handshake after being
  spawned. `ready().await` completes when both peers have exchanged HELLO
  frames and credits are installed. `send()`/`recv()` internally await
  readiness.
- **Deterministic lifecycle**: Dropping the unspawned driver, aborting the
  driver task, or driver failure all transition the frontend to a failed
  state and wake all waiters. `close().await` signals the driver to shut
  down and waits for completion without owning the `JoinHandle`.
- **Pre-posted receives**: All receive buffers (data + control headroom) are
  posted before the CM handshake completes
- **Bounded backpressure**: Both send buffer pool and credit semaphore limit
  concurrent sends; additional senders wait asynchronously
- **`send().await` = local completion**: The send CQE confirms local
  completion, not remote consumption
- **Cancellation safe**: If cancelled before WR posting, the credit permit is
  returned automatically. If cancelled after posting, the MR returns via the
  reclaim queue. Dropping `recv()` leaves the message for the next caller.
- **Shared CQ by default**: One CQ + one driver for both send and recv
  completions; separate-CQ mode available via `.separate_cqs(true)`
- **Completion modes**: `Readiness` (fd/channel-based, lower CPU) or
  `Polling` (direct CQ poll, lower latency)

#### Non-Goals

The following are explicitly out of scope for the v2 message transport:

- `AsyncRead`/`AsyncWrite` byte-stream adapters (future layering)
- tonic/gRPC or quinn/QUIC integration (separate crates)
- Ring transports, atomics, inline data, multi-SGE operations
- Dynamic buffer pool resizing
- UD (Unreliable Datagram) queue pair support

## Prerequisites

```sh
# Ubuntu/Debian
sudo apt install libibverbs-dev librdmacm-dev rdma-core

# For protobuf compilation (tonic tests)
sudo apt install protobuf-compiler
```

## Software RDMA Providers

For development and testing without RDMA hardware, use one of the software providers:

| Provider | Type | Script | Notes |
|---|---|---|---|
| **[siw](https://github.com/torvalds/linux/tree/master/drivers/infiniband/sw/siw)** (Soft-iWARP) | iWARP | `sudo ./scripts/setup-siw.sh` | Recommended for testing; works on any Linux |
| **[rxe](https://github.com/torvalds/linux/tree/master/drivers/infiniband/sw/rxe)** (Soft-RoCE) | InfiniBand/RoCE | `sudo ./scripts/setup-rxe.sh` | Supports atomics and Write+Imm; can build from source via `just build-rxe` |

Both scripts check for kernel modules, load them, create a device, and verify with `ibv_devices`.

If the machine also exposes a hardware RDMA device on the same network interface (e.g. a Mellanox VF or an Azure MANA RDMA function on a cloud VM), `rdma_cm` may bind connections to that device instead of the software one, which breaks same-host tests. On a disposable machine, `sudo ./scripts/unload-hw-rdma.sh` unloads those hardware RDMA drivers (netdev drivers are left alone) so only siw/rxe remain.

> On Azure, in-guest RDMA over the MANA NIC is provided by **Guest RDMA for Azure Boost** (preview) — see [Announcing Preview of Guest RDMA for Azure Boost](https://techcommunity.microsoft.com/blog/azurecompute/announcing-preview-of-guest-rdma-for-azure-boost/4524589). This is the fabric the [Azure MANA RoCEv2 benchmarks](docs/bench/azure-mana-rocev2/README.md) run on.

## Build

```sh
cargo build                    # default (includes tokio async support)
cargo build --no-default-features --features async  # futures-only async, no tokio
```

To build the rxe kernel module from source (optional):

```sh
just build-rxe
```

## Test

Tests require a software RDMA provider (siw or rxe) and must run single-threaded due to kernel resource contention:

```sh
# Set up a provider first
sudo ./scripts/setup-siw.sh

# Run tests
RUST_TEST_THREADS=1 cargo test -p rdma-io-tests -- --nocapture
```

## Documentation

Design documents and background research are in [`docs/`](docs/):

| Document | Description |
|---|---|
| [SafeApi.md](docs/design/SafeApi.md) | Safe API design and RAII ownership model |
| [RdmaOperations.md](docs/design/RdmaOperations.md) | RDMA verb operations and data path patterns |
| [rdma-transport-layer.md](docs/design/rdma-transport-layer.md) | Transport trait architecture and transport implementations |
| [DataPathCopies.md](docs/design/DataPathCopies.md) | Send/recv copy audit of the transport & stream interfaces, and where `Buf`/`Bytes` would help |
| [quinn-rdma.md](docs/design/quinn-rdma.md) | Quinn QUIC over RDMA design (includes tonic-h3 integration) |
| [rdma-transport-comparison.md](docs/design/rdma-transport-comparison.md) | Three-way transport comparison (rdma-io vs msquic vs ring) |
| [TonicRdmaVsTcpPerformance.md](docs/design/TonicRdmaVsTcpPerformance.md) | Why tonic-over-RDMA trails TCP at low concurrency (root-cause analysis) |
| [EchoBenchmark.md](docs/design/EchoBenchmark.md) | Direct transport-level echo benchmark (`--mode echo`) design and diagrams |
| [Testing.md](docs/design/Testing.md) | Test strategy and provider compatibility matrix |
| [RingBufferStream.md](docs/design/RingBufferStream.md) | Ring buffer stream design (RDMA Write alternative) |
| [BndBindings.md](docs/design/BndBindings.md) | FFI binding generation with bnd |
| [siw-vs-rxe.md](docs/background/siw-vs-rxe.md) | Software provider comparison |
| [msquic-rdma.md](docs/background/msquic-rdma.md) | msquic RDMA transport architecture analysis |
| [bench/scenarios/grpc.md](docs/bench/scenarios/grpc.md) | gRPC-over-RDMA (`rh2`) vs TCP benchmark scenario + results |
| [bench/scenarios/h1.md](docs/bench/scenarios/h1.md) | HTTP/1.1 (`rh1`/`tcp1`) benchmark scenario + results |
| [bench/scenarios/echo.md](docs/bench/scenarios/echo.md) | Direct-transport (`--mode echo`) benchmark scenario + results |
| [benchv3/README.md](docs/benchv3/README.md) | Fixed-workload benchmark framework (v3) — comparable, reproducible RDMA-vs-TCP grid |

## CI

CI runs on GitHub Actions with two jobs:

- **build-and-test** — builds on siw, runs clippy, tests (×5 for flakiness), doc tests
- **build-rxe** — builds rxe kernel module from source, tests on Soft-RoCE

## License

MIT
