# rdma-io

Safe Rust bindings for RDMA programming over [libibverbs](https://github.com/linux-rdma/rdma-core) and [librdmacm](https://github.com/linux-rdma/rdma-core).

## Features

- **Safe RAII wrappers** — `ProtectionDomain`, `CompletionQueue`, `MemoryRegion`, `QueuePair`, `CmId`, etc. with `Arc`-based ownership enforcing correct destruction order
- **Sync stream** — `RdmaStream` implements `std::io::Read` + `Write` over RDMA SEND/RECV with `RdmaListener` for TCP-like usage
- **Async stream** — `AsyncRdmaStream` implements `tokio::io::AsyncRead` + `AsyncWrite` (feature `tokio`) and `futures::AsyncRead` + `AsyncWrite`
- **Low-level async** — `AsyncCq` (completion queue polling via epoll) and `AsyncQp` for custom RDMA verb patterns (SEND/RECV, RDMA READ/WRITE, atomics)
- **Generated FFI** — `rdma-io-sys` provides bindings generated with [bnd](https://github.com/nicira/bnd), including wrappers for `ibverbs` inline functions

## Workspace Crates

| Crate | Description |
|---|---|
| `rdma-io` | Safe high-level API |
| `rdma-io-sys` | Raw FFI bindings (`libibverbs` + `librdmacm`) |
| `bnd-rdma-gen` | Binding generator (dev-only) |
| `rdma-io-tests` | Integration tests |

## Quick Start

```rust
use rdma_io::stream::{RdmaListener, RdmaStream};
use std::io::{Read, Write};

// Server
let listener = RdmaListener::bind(&"0.0.0.0:9999".parse().unwrap()).unwrap();
let mut server = listener.accept().unwrap();

// Client (from another thread/process)
let mut client = RdmaStream::connect(&"10.0.0.1:9999".parse().unwrap()).unwrap();
client.write_all(b"hello rdma").unwrap();

let mut buf = [0u8; 1024];
let n = server.read(&mut buf).unwrap();
assert_eq!(&buf[..n], b"hello rdma");
```

## Prerequisites

```sh
# Ubuntu/Debian
sudo apt install libibverbs-dev librdmacm-dev

# For testing without RDMA hardware (software iWARP)
sudo modprobe siw
```

## Build

```sh
cargo build
cargo build --features tokio   # async support
```

## Test

Tests require a software RDMA provider (siw or rxe) and must run single-threaded:

```sh
RUST_TEST_THREADS=1 cargo test -p rdma-io-tests -- --nocapture
```

## License

MIT
