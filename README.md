# rdma-io

Safe Rust bindings for RDMA programming over [libibverbs](https://github.com/linux-rdma/rdma-core) and [librdmacm](https://github.com/linux-rdma/rdma-core), with async support, [tonic](https://github.com/hyperium/tonic) gRPC integration, and [Quinn](https://github.com/quinn-rs/quinn) QUIC integration.

> [!WARNING]
> **⚠️ Experimental** — This library is under active development and is not ready for production use.
> APIs and behavior may change without notice.

> **Primary target:** Azure Guest RDMA over MANA NICs using **Guest RDMA for Azure Boost** (preview). See the [Azure Boost announcement](https://techcommunity.microsoft.com/blog/azurecompute/announcing-preview-of-guest-rdma-for-azure-boost/4524589) and the [Azure MANA RoCEv2 benchmarks](docs/bench/azure-mana-rocev2/README.md) for the tested environment and results.

## Features

- **Async RDMA networking** — Build full-duplex Rust services over Azure Guest RDMA and other libibverbs/librdmacm providers.
- **Application transports** — Use stream-oriented I/O or reusable Send/Recv and RDMA Write transports without managing verbs directly.
- **gRPC and QUIC integration** — Run tonic gRPC, Quinn QUIC, and tonic gRPC over HTTP/3 on RDMA transports.
- **Low-level control when needed** — Access safe resource wrappers and asynchronous RDMA verbs for custom protocols.

## Workspace Crates

| Crate | Description |
|---|---|
| `rdma-io` | Core RDMA networking and transport APIs |
| `rdma-io-tonic` | tonic gRPC over RDMA |
| `rdma-io-quinn` | Quinn QUIC over RDMA |

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
