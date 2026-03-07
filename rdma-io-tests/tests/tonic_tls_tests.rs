//! Tonic gRPC over RDMA with TLS (OpenSSL via tonic-tls) integration tests.
//!
//! Uses [`rcgen`] to generate a self-signed CA, server cert, and client cert
//! at test time. Verifies mTLS gRPC over RDMA end-to-end.

use std::time::Duration;

use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::X509;
use rdma_io::async_stream::AsyncRdmaListener;
use rdma_io_tonic::{RdmaIncoming, RdmaTransport};
use tokio_stream::StreamExt;
use tonic::Request;
use tonic::transport::{Channel, Endpoint, Server};

use rdma_io_tests::greeter_service::*;
use rdma_io_tests::test_helpers::{local_ip, test_addrs};

// ---------------------------------------------------------------------------
// Test certificate generation (rcgen)
// ---------------------------------------------------------------------------

struct TestPki {
    ca_cert_pem: Vec<u8>,
    server_cert_pem: Vec<u8>,
    server_key_pem: Vec<u8>,
    client_cert_pem: Vec<u8>,
    client_key_pem: Vec<u8>,
}

fn generate_test_pki() -> TestPki {
    // CA
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Test CA");
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_cert_pem = ca_cert.pem().into_bytes();

    // Server cert signed by CA
    let server_key = rcgen::KeyPair::generate().unwrap();
    let mut server_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    server_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    // Add the local IP as a SAN so the server cert is valid for the connect addr
    if let Ok(ip) = local_ip().parse::<std::net::IpAddr>() {
        server_params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(ip));
    }
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .unwrap();
    let server_cert_pem = server_cert.pem().into_bytes();
    let server_key_pem = server_key.serialize_pem().into_bytes();

    // Client cert signed by CA
    let client_key = rcgen::KeyPair::generate().unwrap();
    let mut client_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    client_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "test-client");
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .unwrap();
    let client_cert_pem = client_cert.pem().into_bytes();
    let client_key_pem = client_key.serialize_pem().into_bytes();

    TestPki {
        ca_cert_pem,
        server_cert_pem,
        server_key_pem,
        client_cert_pem,
        client_key_pem,
    }
}

// ---------------------------------------------------------------------------
// Server + client setup with TLS
// ---------------------------------------------------------------------------

async fn start_tls_server_and_connect(
    pki: &TestPki,
) -> (
    GreeterClient<Channel>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (bind_addr, connect_addr) = test_addrs();

    // --- Server TLS ---
    let mut acceptor_builder =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).expect("SslAcceptor builder");
    acceptor_builder
        .set_certificate(&X509::from_pem(&pki.server_cert_pem).expect("parse server cert"))
        .expect("set server cert");
    acceptor_builder
        .set_private_key(
            &openssl::pkey::PKey::private_key_from_pem(&pki.server_key_pem)
                .expect("parse server key"),
        )
        .expect("set server key");
    // Require client certificates (mTLS)
    acceptor_builder.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    acceptor_builder
        .cert_store_mut()
        .add_cert(X509::from_pem(&pki.ca_cert_pem).expect("parse CA cert"))
        .expect("add CA to trust store");
    // h2 ALPN for gRPC (wire format: length-prefixed protocol name)
    const ALPN_H2_WIRE: &[u8] = b"\x02h2";
    acceptor_builder
        .set_alpn_protos(ALPN_H2_WIRE)
        .expect("set ALPN");
    let acceptor = acceptor_builder.build();

    let listener = AsyncRdmaListener::bind(&bind_addr).expect("bind RDMA listener");
    let incoming = RdmaIncoming::new(listener);
    let tls_incoming = tonic_tls::openssl::TlsIncoming::new(incoming, acceptor);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(GreeterServer::new(MyGreeter))
            .serve_with_incoming_shutdown(tls_incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    tracing::info!("TLS server listening on {bind_addr}");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- Client TLS ---
    let mut connector_builder =
        SslConnector::builder(SslMethod::tls_client()).expect("SslConnector builder");
    // Trust our test CA
    connector_builder
        .cert_store_mut()
        .add_cert(X509::from_pem(&pki.ca_cert_pem).expect("parse CA cert"))
        .expect("add CA to client trust store");
    // Client certificate (mTLS)
    connector_builder
        .set_certificate(&X509::from_pem(&pki.client_cert_pem).expect("parse client cert"))
        .expect("set client cert");
    connector_builder
        .set_private_key(
            &openssl::pkey::PKey::private_key_from_pem(&pki.client_key_pem)
                .expect("parse client key"),
        )
        .expect("set client key");
    // h2 ALPN
    connector_builder
        .set_alpn_protos(ALPN_H2_WIRE)
        .expect("set ALPN");
    let ssl_conn = connector_builder.build();

    let transport = RdmaTransport::new();
    let ip = connect_addr.ip();
    let port = connect_addr.port();
    // Use IP string as domain for SNI — the server cert has the IP as a SAN.
    let domain = ip.to_string();

    let uri = format!("https://{ip}:{port}");

    tracing::info!("TLS client connecting to {uri}...");
    let mut channel = None;
    for attempt in 1..=3 {
        // Rebuild connector each attempt since TlsConnector doesn't impl Clone.
        let connector = tonic_tls::openssl::TlsConnector::new(
            transport.clone(),
            ssl_conn.clone(),
            domain.clone(),
        );
        match Endpoint::from_shared(uri.clone())
            .unwrap()
            .connect_with_connector(connector)
            .await
        {
            Ok(ch) => {
                channel = Some(ch);
                break;
            }
            Err(e) if attempt < 3 => {
                tracing::warn!("TLS connect attempt {attempt} failed: {e:#}, retrying...");
                // Print full error chain for debugging
                let mut source = std::error::Error::source(&e);
                while let Some(cause) = source {
                    tracing::warn!("  caused by: {cause}");
                    source = std::error::Error::source(cause);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                let mut source = std::error::Error::source(&e);
                while let Some(cause) = source {
                    tracing::error!("  caused by: {cause}");
                    source = std::error::Error::source(cause);
                }
                panic!("TLS RDMA connect failed after {attempt} attempts: {e:#}");
            }
        }
    }
    let client = GreeterClient::new(channel.unwrap());
    tracing::info!("TLS client connected");
    (client, shutdown_tx, server_handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test: unary, server-streaming, and bidi-streaming gRPC over RDMA with mTLS.
///
/// All RPC patterns are exercised against a single TLS server to avoid the siw
/// resource cleanup race that occurs when repeatedly creating and destroying
/// RDMA connections between separate tests.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn tonic_tls_over_rdma() {
    let pki = generate_test_pki();
    let (mut client, shutdown_tx, server_handle) = start_tls_server_and_connect(&pki).await;

    // -- Unary --
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.say_hello(Request::new(HelloRequest {
            name: "TLS-RDMA".into(),
        })),
    )
    .await
    .expect("gRPC unary call timed out")
    .expect("gRPC unary call failed");
    assert_eq!(response.into_inner().message, "Hello TLS-RDMA!");
    tracing::info!("TLS unary call succeeded");

    // -- Server streaming --
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.server_stream(Request::new(HelloRequest { name: "tls".into() })),
    )
    .await
    .expect("gRPC server_stream call timed out")
    .expect("gRPC server_stream call failed");
    let mut stream = response.into_inner();
    let mut messages = Vec::new();
    while let Some(reply) = stream.next().await {
        messages.push(reply.expect("stream error").message);
    }
    assert_eq!(messages, vec!["tls-0", "tls-1", "tls-2", "tls-3", "tls-4"]);
    tracing::info!("TLS server stream succeeded");

    // -- Bidi streaming --
    let request_stream = tokio_stream::iter(["alpha", "beta"].into_iter().map(|n| HelloRequest {
        name: n.to_string(),
    }));
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.bidi_stream(Request::new(request_stream)),
    )
    .await
    .expect("gRPC bidi_stream call timed out")
    .expect("gRPC bidi_stream call failed");
    let mut stream = response.into_inner();
    let mut messages = Vec::new();
    while let Some(reply) = stream.next().await {
        messages.push(reply.expect("stream error").message);
    }
    assert_eq!(messages, vec!["echo: alpha", "echo: beta"]);
    tracing::info!("TLS bidi stream succeeded");

    drop(client);
    let _ = shutdown_tx.send(());
    server_handle.await.unwrap();
}
