# rdma-io-tonic

[tonic](https://github.com/hyperium/tonic) gRPC transport over RDMA.

Drop-in RDMA transport for tonic servers and clients — use `RdmaIncoming` for the server side and `RdmaConnector` for the client side. Supports all gRPC patterns (unary, server/client/bidi streaming) and optional mTLS via OpenSSL.

## Usage

### Server

```rust
use rdma_io_tonic::RdmaIncoming;
use tonic::transport::Server;

let incoming = RdmaIncoming::bind(&"0.0.0.0:50051".parse().unwrap()).await?;
Server::builder()
    .add_service(my_service)
    .serve_with_incoming(incoming)
    .await?;
```

### Client

```rust
use rdma_io_tonic::RdmaConnector;
use tonic::transport::Endpoint;

let connector = RdmaConnector::default();
let channel = Endpoint::from_static("http://10.0.0.1:50051")
    .connect_with_connector(connector)
    .await?;
let client = MyServiceClient::new(channel);
```

### Custom Buffer Size

Both connector and incoming default to 64 KiB RDMA buffers. Override with:

```rust
let connector = RdmaConnector::with_buf_size(128 * 1024);
let incoming = RdmaIncoming::bind_with_buf_size(
    &"0.0.0.0:50051".parse().unwrap(),
    128 * 1024,
).await?;
```

### TLS (mTLS with OpenSSL)

Enable the `tls` feature for TLS support via [`tonic-tls`](https://crates.io/crates/tonic-tls) and OpenSSL:

```toml
[dependencies]
rdma-io-tonic = { version = "0.0.1", features = ["tls"] }
```

**Server:**

```rust
use openssl::ssl::{SslAcceptor, SslMethod, SslVerifyMode};

let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
builder.set_certificate(&server_cert)?;
builder.set_private_key(&server_key)?;
builder.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
builder.set_alpn_protos(b"\x02h2")?;
let acceptor = builder.build();

let incoming = RdmaIncoming::bind(&addr).await?;
let tls_incoming = tonic_tls::openssl::TlsIncoming::new(incoming, acceptor);
Server::builder()
    .add_service(my_service)
    .serve_with_incoming(tls_incoming)
    .await?;
```

**Client:**

```rust
use rdma_io_tonic::RdmaTransport;
use openssl::ssl::{SslConnector, SslMethod};

let mut builder = SslConnector::builder(SslMethod::tls_client())?;
builder.cert_store_mut().add_cert(ca_cert)?;
builder.set_certificate(&client_cert)?;
builder.set_private_key(&client_key)?;
builder.set_alpn_protos(b"\x02h2")?;
let ssl_connector = builder.build();

let transport = RdmaTransport::new();
let connector = tonic_tls::openssl::TlsConnector::new(
    transport,
    ssl_connector,
    "server.example.com".to_string(),
);
let channel = Endpoint::from_shared(uri)?
    .connect_with_connector(connector)
    .await?;
```

> **Note:** ALPN must use wire format (`b"\x02h2"`), not the raw `b"h2"` string.

## Exports

| Type | Role |
|---|---|
| `RdmaConnector` | Client connector — implements `tower::Service<Uri>` |
| `RdmaIncoming` | Server incoming stream — implements `futures::Stream` + `tonic_tls::Incoming` |
| `TokioRdmaStream` | Tokio-compatible RDMA stream wrapper |
| `RdmaConnectInfo` | Connection metadata (remote address), accessible via `Request::extensions()` |
| `RdmaTransport` | TLS transport — implements `tonic_tls::Transport` *(requires `tls` feature)* |

## Features

| Feature | Default | Description |
|---|---|---|
| `tls` | No | Enables mTLS support via `tonic-tls` + OpenSSL |

## Requirements

- Linux with RDMA support (hardware InfiniBand/RoCE, or software providers siw/rxe)
- `libibverbs-dev` and `librdmacm-dev` system packages
- For TLS: OpenSSL development headers (`libssl-dev`)

## License

MIT
