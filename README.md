# rdma-io

Safe Rust bindings for RDMA programming over [libibverbs](https://github.com/linux-rdma/rdma-core) and [librdmacm](https://github.com/linux-rdma/rdma-core), with async support and [tonic](https://github.com/hyperium/tonic) gRPC integration.

## Features

- **Safe RAII wrappers** — `ProtectionDomain`, `CompletionQueue`, `MemoryRegion`, `QueuePair`, `CmId`, etc. with `Arc`-based ownership enforcing correct destruction order
- **Async stream** — `AsyncRdmaStream` implements `tokio::io::AsyncRead` + `AsyncWrite` and `futures::AsyncRead` + `AsyncWrite` with dual completion queues for full-duplex I/O
- **Low-level async** — `AsyncCq` (completion queue polling via epoll) and `AsyncQp` for custom RDMA verb patterns (Send/Recv, RDMA Read/Write, atomics)
- **tonic gRPC transport** — `RdmaConnector` and `RdmaIncoming` for drop-in RDMA transport with tonic, including optional TLS via `tonic-tls` + OpenSSL
- **Generated FFI** — `rdma-io-sys` provides bindings generated with [bnd](https://github.com/youyuanwu/bnd), including wrappers for `ibverbs` inline functions

## Workspace Crates

| Crate | Description |
|---|---|
| `rdma-io` | Safe high-level API (async streams, connection management, QP verbs) |
| `rdma-io-tonic` | tonic gRPC transport over RDMA (connector, incoming, optional TLS) |
| `rdma-io-sys` | Raw FFI bindings (`libibverbs` + `librdmacm`) |
| `bnd-rdma-gen` | Binding generator (dev-only) |
| `rdma-io-tests` | Integration tests (streams, QP verbs, tonic gRPC, TLS) |

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
