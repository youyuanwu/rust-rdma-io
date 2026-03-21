# Encryption over RDMA

## Problem

RDMA bypasses the kernel networking stack, so kernel TLS (kTLS) does not apply.
By default, RDMA traffic is **unencrypted**. This is acceptable in trusted
datacenter networks but not when confidentiality or integrity is required.

## TLS Over RDMA

TLS only requires a reliable, ordered byte stream. Since `AsyncRdmaStream`
implements `AsyncRead` + `AsyncWrite`, we can layer TLS directly on top:

```
tonic / gRPC (application)
  ↕ plaintext
OpenSSL (TLS 1.3)
  ↕ TLS records (ciphertext)
AsyncRdmaStream (RDMA SEND/RECV)
  ↕ kernel bypass
RDMA NIC hardware
```

## Option 1: OpenSSL + tonic-tls (Implemented)

### Why OpenSSL

| Aspect        | OpenSSL                            | rustls                         |
|---------------|------------------------------------|--------------------------------|
| **Maturity**  | Industry standard, decades proven  | Newer, growing adoption        |
| **FIPS**      | Built-in FIPS provider             | Via `aws-lc-rs` (indirect)     |
| **Ciphers**   | All TLS versions/ciphers           | TLS 1.2+ only, limited set    |
| **Dependencies** | System libssl (`openssl` crate) | Pure Rust (`ring`/`aws-lc-rs`) |

### Implementation (rdma-io-tonic `tls` feature)

The `tls` feature in `rdma-io-tonic` integrates with
[tonic-tls](https://github.com/youyuanwu/tonic-tls) v0.7 (OpenSSL backend):

- **`RdmaTransport`** — implements `tonic_tls::Transport`, produces `TokioRdmaStream`
  from a URI (see `rdma-io-tonic/src/tls.rs`)
- **`RdmaIncoming`** — implements `tonic_tls::Incoming` (one-line trait impl)
- **`TokioRdmaStream`** — has `Send + Sync + Debug + Unpin` bounds required by tonic-tls

The transport stack:

```
tonic gRPC
  → tonic_tls::openssl::TlsConnector / TlsIncoming
    → tokio_openssl::SslStream<TokioRdmaStream>    (TLS)
      → TokioRdmaStream (Compat<AsyncRdmaStream>)  (byte stream)
        → RDMA SEND/RECV verbs                      (kernel bypass)
```

### Client

```rust
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io_tonic::tls::RdmaTransport;
use tonic_tls::openssl::TlsConnector;

let transport = RdmaTransport::new(SendRecvConfig::stream());
let ssl = SslConnector::builder(SslMethod::tls_client())?.build();
let connector = TlsConnector::new(transport, ssl, "server.example.com".into());

let channel = Endpoint::from_static("https://10.0.0.1:50051")
    .connect_with_connector(connector).await?;
let client = MyServiceClient::new(channel);
```

### Server

```rust
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io_tonic::RdmaIncoming;
use tonic_tls::openssl::TlsIncoming;

let acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
// ... set cert, key, CA ...
let incoming = RdmaIncoming::bind(&"0.0.0.0:50051".parse()?, SendRecvConfig::stream())?;
let tls_incoming = TlsIncoming::new(incoming, acceptor.build());

Server::builder()
    .add_service(my_service)
    .serve_with_incoming(tls_incoming).await?;
```

### mTLS Certificate Access

The `SslStream` wrapper implements tonic's `Connected` trait, exposing
`SslConnectInfo` with `peer_certs()` in request extensions:

```rust
let info = request.extensions()
    .get::<tonic_tls::openssl::SslConnectInfo<RdmaConnectInfo>>()
    .ok_or(Status::unauthenticated("no TLS"))?;
let certs = info.peer_certs()
    .ok_or(Status::unauthenticated("no client cert"))?;
```

### Dependencies

```toml
# rdma-io-tonic/Cargo.toml
[features]
tls = ["dep:tonic-tls", "dep:openssl"]

[dependencies]
tonic-tls = { version = "0.7", features = ["openssl"], optional = true }
openssl = { version = "0.10", optional = true }
```

System requirement: `libssl-dev` (Debian/Ubuntu) or `openssl-devel` (RHEL).

## Option 2: rustls + tokio-rustls (Alternative)

`tokio-rustls` wraps any `AsyncRead + AsyncWrite + Unpin` stream.
Same integration pattern — swap OpenSSL for rustls.
No FIPS provider built in; FIPS requires `aws-lc-rs` backend.

```toml
tokio-rustls = "0.26"
rustls = "0.23"
```

## Option 3: Application-Level Encryption (No TLS)

Skip TLS and encrypt at the application layer (e.g. AES-GCM with pre-shared keys).

**Pros:** No TLS record framing overhead, full control over encryption boundaries.
**Cons:** Must implement key exchange, no standard authentication, not compatible
with tonic/gRPC TLS expectations. Custom crypto is easy to get wrong.

**When to use:** Performance-critical paths where you control both endpoints
and can distribute keys out-of-band.

## Option 4: RDMA-Native Crypto (TLS Handshake + RDMA Verbs for Data)

### Problem with TLS over Byte Stream

Layering TLS on `AsyncRdmaStream` introduces inefficiencies:

1. **3–4 memory copies per direction** — TLS has its own buffers, RDMA has MRs
2. **16 KiB record limit** — cannot match RDMA's native 64 KiB+ buffers
3. **Byte-stream mismatch** — RDMA is message-based; we convert
   messages → byte stream → TLS records → byte stream → messages
4. **No one-sided ops** — TLS requires both sides to process every record,
   so RDMA WRITE (zero-copy on receiver) cannot be used

### Hybrid Approach

Use TLS for auth/key exchange, then switch to RDMA-native encrypted data:

```
Phase 1: TLS handshake (SEND/RECV)           → standard TLS, small messages
Phase 2: Key export (TLS-EKM, RFC 5705)      → derive AES-GCM session keys
Phase 3: Encrypted data (RDMA SEND or WRITE) → app-level AES-GCM over verbs
```

#### Option A: SEND/RECV with In-Place AES-GCM

```
Sender:  plaintext in MR → AES-GCM encrypt in-place → ibv_post_send(SEND)
Receiver: NIC DMA into MR → AES-GCM decrypt in-place → plaintext
```

1 copy per direction (vs 3–4 with TLS). No record size limit. Message
boundaries preserved.

#### Option B: RDMA WRITE + SEND Signaling

Sender WRITEs ciphertext directly into remote MR, then SENDs metadata
(offset, length, nonce, tag). Receiver-side zero-copy on the wire.

**Trade-offs:** Requires exchanging rkeys (security-sensitive), more complex
flow control, not supported on iWARP/siw (WRITE with Immediate is IB-only).

### Comparison

| Aspect                 | TLS Byte Stream   | RDMA-Native (A)          | RDMA WRITE (B)           |
|------------------------|-------------------|--------------------------|--------------------------|
| Copies per direction   | 3–4               | 1 (in-place)             | 1 (in-place)             |
| Max encrypt block      | 16 KiB            | Unlimited                | Unlimited                |
| Message boundaries     | Lost              | ✅ Preserved             | ✅ Preserved             |
| gRPC/tonic compatible  | ✅ Yes            | ❌ Custom transport      | ❌ Custom transport      |
| iWARP/siw support      | ✅ Yes            | ✅ Yes                   | ⚠️ Limited               |
| Complexity             | Low               | Medium                   | High                     |

### Recommendation

**Start with TLS byte stream (Option 1)** — it works today and is tonic-compatible.
**Evolve to Option 4A** when profiling shows TLS overhead matters for non-tonic
workloads, via an `EncryptedRdmaStream` that does TLS handshake then switches to
AES-GCM over SEND/RECV.

## Option 5: Hardware Crypto Offload

Modern RDMA NICs support inline encryption:

| NIC              | Crypto              | Line Rate    |
|------------------|---------------------|--------------|
| ConnectX-6 Dx    | TLS, IPsec AES-GCM  | 25–200 Gb/s |
| ConnectX-7       | TLS, IPsec, MACsec  | 400 Gb/s    |
| Chelsio T6       | kTLS, IPsec AES-GCM | 100 Gb/s    |

### By Transport

| Transport   | Encryption                      | Zero-Copy? | Transparent? |
|-------------|---------------------------------|------------|--------------|
| InfiniBand  | ❌ None native (P_Keys = ACL)   | N/A        | N/A          |
| RoCEv2      | ✅ IPsec offload (CX-6Dx+)     | ✅ Yes     | ✅ (ip xfrm) |
| iWARP       | ✅ kTLS offload (Chelsio T6)    | ✅ Yes     | ✅ (kTLS)    |
| siw         | ❌ Software only                | ❌ No      | ❌ No        |

### Why IPsec Alone Is Not Enough

IPsec provides wire encryption but **not application-level authentication**:

| Capability                    | IPsec       | TLS (mTLS)         |
|-------------------------------|-------------|---------------------|
| Wire encryption               | ✅          | ✅                  |
| Host authentication           | ✅ (IKEv2)  | ✅ (server cert)    |
| Client identity (app-level)   | ❌          | ✅ (client cert)    |
| Per-connection auth           | ❌          | ✅                  |
| Visible to application code   | ❌          | ✅ (peer certs API) |
| Works with tonic interceptors | ❌          | ✅                  |

**Use hardware offload as a complement to TLS, not a replacement.**

## Performance Impact

| Approach                    | Latency   | Throughput    | Source            |
|-----------------------------|-----------|---------------|-------------------|
| Software AES-GCM (OpenSSL) | +15–30%   | −40–60%       | sRDMA (ATC '20)   |
| NIC inline crypto (CX-6Dx) | <9%       | Near line rate| sRDMA (ATC '20)   |
| IPsec offload (CX-6+)      | <5%       | Line rate     | NVIDIA datasheets |

Software TLS overhead breakdown:
- **CPU:** AES-128-GCM with AES-NI: ~0.5–1 cycle/byte (~1.5 GB/s single-core)
- **Copies:** +1 memcpy per direction (plaintext → ciphertext in TLS buffer)
- **Framing:** 13-byte header per 16 KiB record (~0.08% overhead)

## References

- [tonic-tls](https://github.com/youyuanwu/tonic-tls) (v0.7.0) — Transport-agnostic TLS for tonic
- [openssl crate](https://docs.rs/openssl) — Rust OpenSSL bindings
- [tokio-openssl](https://docs.rs/tokio-openssl) — Async TLS for tokio via OpenSSL
- [sRDMA (USENIX ATC '20)](https://www.usenix.org/conference/atc20/presentation/taranov) — RDMA encryption
- [Secure RDMA (CoNEXT '25)](https://yoon.ws/publication/secure-rdma-conext25.pdf) — Crypto offloading
- [NVIDIA InfiniBand Security](https://docs.nvidia.com/networking/display/nvidiainfinibandsecurityoverviewandguidelines) — P_Key model
- [Chelsio T6 Crypto](https://www.chelsio.com/crypto-solution/) — iWARP + kTLS offload
