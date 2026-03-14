# rdma-io

Safe Rust bindings for RDMA programming over [libibverbs](https://github.com/linux-rdma/rdma-core) and [librdmacm](https://github.com/linux-rdma/rdma-core), with async support, [tonic](https://github.com/hyperium/tonic) gRPC integration, and [Quinn](https://github.com/quinn-rs/quinn) QUIC integration.

## Features

- **Safe RAII wrappers** — `ProtectionDomain`, `CompletionQueue`, `MemoryRegion`, `QueuePair`, `CmId`, etc. with `Arc`-based ownership enforcing correct destruction order
- **Async stream** — `AsyncRdmaStream` implements `tokio::io::AsyncRead` + `AsyncWrite` and `futures::AsyncRead` + `AsyncWrite` with dual completion queues for full-duplex I/O
- **Transport trait** — Generic `Transport` abstraction decoupling RDMA mechanics from consumers; `RdmaTransport` provides the concrete implementation
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
use rdma_io::async_stream::{AsyncRdmaListener, AsyncRdmaStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Server
let listener = AsyncRdmaListener::bind("0.0.0.0:0").await.unwrap();
let addr = listener.local_addr().unwrap();
let (mut server, _) = listener.accept().await.unwrap();

// Client
let mut client = AsyncRdmaStream::connect(&addr).await.unwrap();
client.write_all(b"hello async rdma").await.unwrap();
```

### tonic gRPC over RDMA

```rust
use rdma_io_tonic::{RdmaConnector, RdmaIncoming};

// Server
let incoming = RdmaIncoming::bind("0.0.0.0:50051").await.unwrap();
Server::builder()
    .add_service(my_service)
    .serve_with_incoming(incoming).await?;

// Client
let connector = RdmaConnector;
let channel = Endpoint::from_static("http://10.0.0.1:50051")
    .connect_with_connector(connector).await?;
```

### Quinn QUIC over RDMA

```rust
use rdma_io_quinn::RdmaUdpSocket;
use quinn::{Endpoint, EndpointConfig};

// Server: bind RDMA socket, create Quinn endpoint
let server_socket = Arc::new(RdmaUdpSocket::bind(&"0.0.0.0:0".parse().unwrap())?);
let server_endpoint = Endpoint::new_with_abstract_socket(
    EndpointConfig::default(), Some(server_config),
    server_socket, runtime,
)?;
let incoming = server_endpoint.accept().await.unwrap();

// Client: pre-connect RDMA, then create Quinn endpoint
let client_socket = RdmaUdpSocket::bind(&"0.0.0.0:0".parse().unwrap())?;
client_socket.connect_to(&server_addr, TransportConfig::datagram()).await?;
let client_endpoint = Endpoint::new_with_abstract_socket(
    EndpointConfig::default(), None,
    Arc::new(client_socket), runtime,
)?;
let connection = client_endpoint.connect(server_addr, "localhost")?.await?;
```

### tonic gRPC over HTTP/3 over RDMA

```rust
use rdma_io_quinn::RdmaUdpSocket;
use tonic_h3::quinn::{H3QuinnAcceptor, H3QuinnConnector};

// Server: RDMA socket → Quinn endpoint → tonic-h3 acceptor
let socket = Arc::new(RdmaUdpSocket::bind(&addr)?);
let endpoint = Endpoint::new_with_abstract_socket(config, Some(h3_server_config), socket, rt)?;
let acceptor = H3QuinnAcceptor::new(endpoint);
tonic_h3::server::H3Router::new(routes).serve(acceptor).await?;

// Client: pre-connect RDMA → Quinn endpoint → H3 channel → gRPC client
let socket = RdmaUdpSocket::bind(&addr)?;
socket.connect_to(&server_addr, TransportConfig::datagram()).await?;
let endpoint = Endpoint::new_with_abstract_socket(config, None, Arc::new(socket), rt)?;
let connector = H3QuinnConnector::new(uri.clone(), "localhost".into(), endpoint);
let channel = tonic_h3::H3Channel::new(connector, uri);
let client = GreeterClient::new(channel);
```

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
| **siw** (Soft-iWARP) | iWARP | `sudo ./scripts/setup-siw.sh` | Recommended for testing; works on any Linux |
| **rxe** (Soft-RoCE) | InfiniBand/RoCE | `sudo ./scripts/setup-rxe.sh` | Supports atomics and Write+Imm; can build from source via CMake |

Both scripts check for kernel modules, load them, create a device, and verify with `ibv_devices`.

## Build

```sh
cargo build                    # default (includes tokio async support)
cargo build --no-default-features --features async  # futures-only async, no tokio
```

To build the rxe kernel module from source (optional):

```sh
cmake -B build -DBUILD_RXE=ON
cmake --build build --target rxe
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
| [rdma-transport-layer.md](docs/design/rdma-transport-layer.md) | Transport trait architecture and RdmaTransport |
| [quinn-rdma.md](docs/design/quinn-rdma.md) | Quinn QUIC over RDMA design (includes tonic-h3 integration) |
| [rdma-transport-comparison.md](docs/design/rdma-transport-comparison.md) | Three-way transport comparison (rdma-io vs msquic vs ring) |
| [Testing.md](docs/design/Testing.md) | Test strategy and provider compatibility matrix |
| [RingBufferStream.md](docs/design/RingBufferStream.md) | Ring buffer stream design (RDMA Write alternative) |
| [BndBindings.md](docs/design/BndBindings.md) | FFI binding generation with bnd |
| [siw-vs-rxe.md](docs/background/siw-vs-rxe.md) | Software provider comparison |
| [msquic-rdma.md](docs/background/msquic-rdma.md) | msquic RDMA transport architecture analysis |

## CI

CI runs on GitHub Actions with two jobs:

- **build-and-test** — builds on siw, runs clippy, tests (×5 for flakiness), doc tests
- **build-rxe** — builds rxe kernel module from source, tests on Soft-RoCE

## License

MIT
