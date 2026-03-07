# Tonic gRPC over RDMA — Design

> Running [tonic](https://docs.rs/tonic) gRPC services over RDMA instead of TCP.
> Users define `.proto` services normally, then swap the transport layer.
>
> **Status: Implemented** in the `rdma-io-tonic` crate.

---

## 1. Background

### The gap

`AsyncRdmaStream` implements `futures_io::AsyncRead/Write`. Tonic needs
different traits on each side:

| Requirement             | We have                        | Need                                      |
|-------------------------|--------------------------------|-------------------------------------------|
| Server IO traits        | `futures_io::AsyncRead/Write`  | `tokio::io::AsyncRead/Write` + `Connected`|
| Client connector IO     | `futures_io::AsyncRead/Write`  | `hyper::rt::Read/Write`                   |
| Server incoming stream  | `listener.accept()` (method)   | `Stream<Item = Result<IO, Error>>`        |
| Client connector        | `Stream::connect()` (method)   | `tower::Service<Uri>`                     |

---

## 2. Architecture

```
┌─────────────────────────────────────────────────┐
│                   User Code                     │
│          .proto service + tonic codegen          │
└──────────┬──────────────────────────┬───────────┘
           │ Server                   │ Client
           ▼                          ▼
┌────────────────────┐   ┌───────────────────────┐
│ serve_with_incoming│   │connect_with_connector  │
│  RdmaIncoming      │   │  RdmaConnector         │
│  (Stream<Item=     │   │  (Service<Uri> →       │
│   Result<IO,E>>)   │   │   TokioIo<TokioRdma>)  │
└────────┬───────────┘   └──────────┬────────────┘
         │                          │
         ▼                          ▼
┌─────────────────────────────────────────────────┐
│          TokioRdmaStream (newtype)              │
│   tokio::io::AsyncRead/Write + Connected        │
│   wraps: Compat<AsyncRdmaStream>                │
└─────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────┐
│             AsyncRdmaStream                     │
│   futures_io::AsyncRead/Write                   │
│   RDMA SEND/RECV via AsyncQp + AsyncCq          │
└─────────────────────────────────────────────────┘
```

The IO adapter chain (client side) bridges three trait families:

```
AsyncRdmaStream            → futures_io::AsyncRead/Write
  → Compat<AsyncRdmaStream>  → tokio::io::AsyncRead/Write
    → TokioRdmaStream          + Connected (newtype)
      → TokioIo<TokioRdmaStream> → hyper::rt::Read/Write  ← tonic needs this
```

### Crate structure

`rdma-io-tonic` is a separate workspace crate, keeping tonic/hyper/tower
dependencies out of the core `rdma-io` crate.

```
rdma-io-tonic/src/
├── lib.rs          # Re-exports: RdmaConnector, RdmaIncoming, TokioRdmaStream, RdmaConnectInfo
├── stream.rs       # TokioRdmaStream + RdmaConnectInfo + Connected impl
├── incoming.rs     # RdmaIncoming (Stream impl over AsyncRdmaListener)
└── connector.rs    # RdmaConnector (Service<Uri> impl)
```

---

## 3. Design Decisions

### Why a newtype (`TokioRdmaStream`) instead of direct tokio trait impls?

1. `AsyncRdmaStream` uses `futures_io` for runtime independence — adding
   tokio traits couples it to tokio.
2. `Connected` is tonic-specific and doesn't belong on the core stream.
3. Zero-cost: `Compat` is a transparent wrapper.

### Accept loop: boxed future vs poll-based

`RdmaIncoming` stores a `Pin<Box<dyn Future>>` per accept (Option A) rather
than a hand-rolled poll state machine (Option B). The accept flow is a
multi-step sequence (CONNECT_REQUEST → allocate PD/CQ/QP/MRs → handshake →
ESTABLISHED → migrate). Each accept already allocates 3 × 64 KiB of
registered memory; one `Box<Future>` (~64 bytes) is noise in comparison.

### Send safety

- `TokioRdmaStream`: explicit `unsafe impl Send` (AsyncRdmaStream is Send).
- `RdmaIncoming`: explicit `unsafe impl Send`. Uses a `ListenerPtr` newtype
  to make the raw pointer to the listener Send-safe within the boxed accept
  future. Only one accept future is alive at a time.

### URI parsing

The connector extracts `host:port` from any URI scheme (`http://`, `rdma://`,
etc.) and strips IPv6 brackets. The `rdma_cm` layer handles RDMA address
resolution transparently from the IP.

### Buffer sizes

Default 64 KiB aligns well with HTTP/2 (16 KiB default max frame size).
Configurable via `RdmaConnector::with_buf_size()` and
`RdmaIncoming::bind_with_buf_size()`.

### Connection pooling

Tonic's `Channel` reconnects automatically via `Service::call`. Each call
creates a fresh `AsyncRdmaStream`. RDMA connections are heavier than TCP
(address/route resolution + RC QP setup) — a connection pool may be needed
if churn becomes an issue.

### Sync bound

Tonic 0.14 only requires `Send`, not `Sync`. Our streams are `Send` but not
`Sync` (single-owner semantics). Fine — tonic spawns one task per connection.

---

## 4. API Surface

### Server

```rust
use rdma_io_tonic::{RdmaIncoming, RdmaConnectInfo};
use tonic::transport::Server;

let incoming = RdmaIncoming::bind(&"0.0.0.0:50051".parse().unwrap())?;

Server::builder()
    .add_service(GreeterServer::new(my_greeter))
    .serve_with_incoming(incoming)
    .await?;

// In handler — access RDMA connection info:
if let Some(info) = request.extensions().get::<RdmaConnectInfo>() {
    println!("RDMA peer: {:?}", info.remote_addr);
}
```

### Client

```rust
use rdma_io_tonic::RdmaConnector;
use tonic::transport::Endpoint;

let connector = RdmaConnector::new();  // default 64 KiB buffers
let channel = Endpoint::from_static("http://10.0.0.1:50051")
    .connect_with_connector(connector)
    .await?;

let mut client = GreeterClient::new(channel);
```

### Lazy connection (recommended for production)

```rust
let connector = RdmaConnector::with_buf_size(256 * 1024);
let channel = Endpoint::from_static("http://10.0.0.1:50051")
    .connect_with_connector_lazy(connector);
// Connection established on first RPC
```

---

## 5. Testing

Tests live in `rdma-io-tests/src/tonic_tests.rs` — a full gRPC Greeter
round-trip over RDMA using siw (Soft-iWARP).

**Test patterns:**
- Port 0 allocation (OS picks free port) to avoid conflicts.
- Connect retry (3 attempts, 500 ms backoff) for siw transient failures.
- 5-second timeout on gRPC calls.
- Graceful shutdown via oneshot channel + `serve_with_incoming_shutdown`,
  with 2-second abort fallback.
- 100 ms cooldown after shutdown for siw kernel resource cleanup.

**Proto compilation** uses `tonic-prost-build` 0.14 (not the older
`tonic-build`).

| Test                        | What it verifies                          | Status  |
|-----------------------------|-------------------------------------------|---------|
| `tonic_greeter_over_rdma`   | Unary RPC round-trip + graceful shutdown  | ✅ Done |
| `tonic_streaming_over_rdma` | Server-streaming RPC                      | Planned |
| `tonic_bidi_over_rdma`      | Bidirectional streaming RPC               | Planned |
| `tonic_connect_info`        | `RdmaConnectInfo` in request extensions   | Planned |

---

## 6. Dependencies

All tonic-related deps are isolated in `rdma-io-tonic`; nothing added to
core `rdma-io`.

| Crate           | Version | Purpose                              |
|-----------------|---------|--------------------------------------|
| `tonic`         | 0.14    | gRPC framework, `Connected` trait    |
| `hyper-util`    | 0.1     | `TokioIo` (tokio↔hyper IO bridge)   |
| `tower-service` | 0.3     | `Service<Uri>` trait for connector   |
| `http`          | 1.x     | `Uri` type                           |
| `tokio-util`    | ws      | `Compat` (futures↔tokio IO bridge)   |
| `futures-util`  | ws      | `Stream` trait                       |
