//! tonic gRPC over HTTP/3 (QUIC) over RDMA tests.
//!
//! Full stack: tonic gRPC → HTTP/3 (h3) → QUIC (Quinn) → RDMA (rdma-io).
//! Each test has a generic body and two concrete entry points:
//! `_default` (Send/Recv) and `_ring` (Ring buffer, skipped on iWARP).

use std::sync::Arc;

use quinn::{Endpoint, EndpointConfig};
use rdma_io::rdma_ring_transport::RingConfig;
use rdma_io::rdma_transport::TransportConfig;
use rdma_io::transport::TransportBuilder;
use rdma_io_quinn::RdmaUdpSocket;

use rdma_io_tests::greeter_service::*;
use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers;

fn generate_self_signed_cert() -> (
    Vec<quinn::rustls::pki_types::CertificateDer<'static>>,
    quinn::rustls::pki_types::PrivateKeyDer<'static>,
) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key =
        quinn::rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let cert = quinn::rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
    (vec![cert], key.into())
}

/// Build a Quinn server config with ALPN "h3" for HTTP/3.
fn make_h3_server_config(
    certs: &[quinn::rustls::pki_types::CertificateDer<'static>],
    key: quinn::rustls::pki_types::PrivateKeyDer<'static>,
) -> quinn::ServerConfig {
    let mut tls_config = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs.to_vec(), key)
        .unwrap();
    tls_config.alpn_protocols = vec![b"h3".to_vec()];
    tls_config.max_early_data_size = u32::MAX;

    quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap(),
    ))
}

/// Build a Quinn client config with ALPN "h3" and proper cert verification.
fn make_h3_client_config(
    server_certs: &[quinn::rustls::pki_types::CertificateDer<'static>],
) -> quinn::ClientConfig {
    let mut roots = quinn::rustls::RootCertStore::empty();
    for cert in server_certs {
        roots.add(cert.clone()).unwrap();
    }
    let mut tls_config = quinn::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls_config).unwrap(),
    ))
}

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

/// Start a tonic-h3 gRPC server on an RDMA-backed Quinn endpoint.
async fn start_h3_server<B: TransportBuilder>(
    certs: &[quinn::rustls::pki_types::CertificateDer<'static>],
    key: quinn::rustls::pki_types::PrivateKeyDer<'static>,
    builder: B,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let server_config = make_h3_server_config(certs, key);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let server_socket =
        Arc::new(RdmaUdpSocket::bind(&test_helpers::bind_addr(), builder).expect("server bind"));
    let connect_addr = test_helpers::connect_addr_for(Some(server_socket.bound_addr()));

    let server_endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(server_config),
        server_socket,
        runtime,
    )
    .expect("server endpoint");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_ep = server_endpoint.clone();
    let server_task = tokio::spawn(async move {
        let acceptor = tonic_h3::quinn::H3QuinnAcceptor::new(server_ep.clone());
        let routes = tonic::service::Routes::builder()
            .add_service(GreeterServer::new(MyGreeter))
            .clone()
            .routes();
        tonic_h3::server::H3Router::new(routes)
            .serve_with_shutdown(acceptor, async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("tonic-h3 server error");
        server_ep.close(0u16.into(), b"shutdown");
        server_ep.wait_idle().await;
    });

    (connect_addr, shutdown_tx, server_task)
}

/// Create an RDMA-backed Quinn client endpoint pre-connected to the server.
async fn make_h3_client_endpoint<B: TransportBuilder>(
    connect_addr: &std::net::SocketAddr,
    client_config: quinn::ClientConfig,
    builder: B,
) -> Endpoint {
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let client_socket =
        RdmaUdpSocket::bind(&test_helpers::bind_addr(), builder).expect("client bind");
    client_socket
        .connect_to(connect_addr)
        .await
        .expect("RDMA pre-connect");

    let mut endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        Arc::new(client_socket),
        runtime,
    )
    .expect("client endpoint");
    endpoint.set_default_client_config(client_config);
    endpoint
}

/// Create a tonic GreeterClient over the HTTP/3 + RDMA channel.
fn make_h3_grpc_client(
    endpoint: &Endpoint,
    connect_addr: &std::net::SocketAddr,
) -> GreeterClient<tonic_h3::H3Channel<tonic_h3::quinn::H3QuinnConnector>> {
    let uri: http::Uri = format!("https://{connect_addr}").parse().unwrap();
    let connector = tonic_h3::quinn::H3QuinnConnector::new(
        uri.clone(),
        "localhost".to_string(),
        endpoint.clone(),
    );
    let channel = tonic_h3::H3Channel::new(connector, uri);
    GreeterClient::new(channel)
}

// ===========================================================================
// h3_unary — unary gRPC over HTTP/3 + RDMA.
// ===========================================================================

async fn h3_unary<B: TransportBuilder>(builder: B) {
    let (certs, key) = generate_self_signed_cert();
    let client_config = make_h3_client_config(&certs);

    let (connect_addr, shutdown_tx, server_task) =
        start_h3_server(&certs, key, builder.clone()).await;
    let client_endpoint = make_h3_client_endpoint(&connect_addr, client_config, builder).await;
    let mut client = make_h3_grpc_client(&client_endpoint, &connect_addr);

    let response = client
        .say_hello(tonic::Request::new(HelloRequest {
            name: "RDMA-H3".into(),
        }))
        .await
        .expect("unary RPC failed");

    let message = response.into_inner().message;
    println!("h3_unary: {message}");
    assert_eq!(message, "Hello RDMA-H3!");

    let _ = shutdown_tx.send(());
    client_endpoint.close(0u16.into(), b"done");
    server_task.await.expect("server task");
}

#[tokio::test]
async fn h3_unary_default() {
    h3_unary(TransportConfig::datagram()).await;
}

#[tokio::test]
async fn h3_unary_ring() {
    require_no_iwarp!();
    h3_unary(RingConfig::datagram()).await;
}

// ===========================================================================
// h3_server_stream — server streaming gRPC over HTTP/3 + RDMA.
// ===========================================================================

async fn h3_server_stream<B: TransportBuilder>(builder: B) {
    let (certs, key) = generate_self_signed_cert();
    let client_config = make_h3_client_config(&certs);

    let (connect_addr, shutdown_tx, server_task) =
        start_h3_server(&certs, key, builder.clone()).await;
    let client_endpoint = make_h3_client_endpoint(&connect_addr, client_config, builder).await;
    let mut client = make_h3_grpc_client(&client_endpoint, &connect_addr);

    let response = client
        .server_stream(tonic::Request::new(HelloRequest {
            name: "stream".into(),
        }))
        .await
        .expect("server_stream RPC failed");

    let mut stream = response.into_inner();
    let mut messages = Vec::new();
    while let Some(reply) = stream.message().await.expect("stream recv") {
        messages.push(reply.message);
    }

    println!("h3_server_stream: {messages:?}");
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0], "stream-0");
    assert_eq!(messages[4], "stream-4");

    let _ = shutdown_tx.send(());
    client_endpoint.close(0u16.into(), b"done");
    server_task.await.expect("server task");
}

#[tokio::test]
async fn h3_server_stream_default() {
    h3_server_stream(TransportConfig::datagram()).await;
}

#[tokio::test]
async fn h3_server_stream_ring() {
    require_no_iwarp!();
    h3_server_stream(RingConfig::datagram()).await;
}

// ===========================================================================
// h3_multi_peer — concurrent clients over HTTP/3 + RDMA.
// ===========================================================================

async fn h3_multi_peer<B: TransportBuilder>(builder: B) {
    let (certs, key) = generate_self_signed_cert();

    let (connect_addr, shutdown_tx, server_task) =
        start_h3_server(&certs, key, builder.clone()).await;

    let client_config_1 = make_h3_client_config(&certs);
    let client_config_2 = make_h3_client_config(&certs);
    let ep1 = make_h3_client_endpoint(&connect_addr, client_config_1, builder.clone()).await;
    let ep2 = make_h3_client_endpoint(&connect_addr, client_config_2, builder).await;
    let mut client1 = make_h3_grpc_client(&ep1, &connect_addr);
    let mut client2 = make_h3_grpc_client(&ep2, &connect_addr);

    let (r1, r2) = tokio::join!(
        client1.say_hello(tonic::Request::new(HelloRequest {
            name: "peer-1".into(),
        })),
        client2.say_hello(tonic::Request::new(HelloRequest {
            name: "peer-2".into(),
        })),
    );
    assert_eq!(
        r1.expect("peer-1 RPC").into_inner().message,
        "Hello peer-1!"
    );
    assert_eq!(
        r2.expect("peer-2 RPC").into_inner().message,
        "Hello peer-2!"
    );
    println!("multi_peer: both concurrent calls succeeded");

    drop(client1);
    ep1.close(0u16.into(), b"peer1 done");

    let r3 = client2
        .say_hello(tonic::Request::new(HelloRequest {
            name: "peer-2-again".into(),
        }))
        .await
        .expect("peer-2 post-disconnect RPC");
    assert_eq!(r3.into_inner().message, "Hello peer-2-again!");
    println!("multi_peer: peer-2 works after peer-1 disconnect");

    let stream_resp = client2
        .server_stream(tonic::Request::new(HelloRequest {
            name: "multi".into(),
        }))
        .await
        .expect("server_stream RPC");
    let mut stream = stream_resp.into_inner();
    let mut count = 0;
    while let Some(reply) = stream.message().await.expect("stream msg") {
        println!("  stream[{count}]: {}", reply.message);
        count += 1;
    }
    assert_eq!(count, 5);
    println!("multi_peer: server-stream on surviving client passed");

    let _ = shutdown_tx.send(());
    ep2.close(0u16.into(), b"peer2 done");
    server_task.await.expect("server task");
}

#[tokio::test]
async fn h3_multi_peer_default() {
    h3_multi_peer(TransportConfig::datagram()).await;
}

#[tokio::test]
async fn h3_multi_peer_ring() {
    require_no_iwarp!();
    h3_multi_peer(RingConfig::datagram()).await;
}

// ===========================================================================
// h3_channel_reuse — send multiple sequential requests on the same channel.
// ===========================================================================

async fn h3_channel_reuse<B: TransportBuilder>(builder: B) {
    let (certs, key) = generate_self_signed_cert();
    let client_config = make_h3_client_config(&certs);

    let (connect_addr, shutdown_tx, server_task) =
        start_h3_server(&certs, key, builder.clone()).await;
    let client_endpoint = make_h3_client_endpoint(&connect_addr, client_config, builder).await;
    let mut client = make_h3_grpc_client(&client_endpoint, &connect_addr);

    // Send multiple requests on the same channel
    for i in 0..5 {
        let response = client
            .say_hello(tonic::Request::new(HelloRequest {
                name: format!("request-{i}"),
            }))
            .await
            .unwrap_or_else(|_| panic!("RPC {i} failed"));

        let message = response.into_inner().message;
        println!("h3_channel_reuse[{i}]: {message}");
        assert_eq!(message, format!("Hello request-{i}!"));
    }

    let _ = shutdown_tx.send(());
    client_endpoint.close(0u16.into(), b"done");
    server_task.await.expect("server task");
}

#[tokio::test]
async fn h3_channel_reuse_default() {
    h3_channel_reuse(TransportConfig::datagram()).await;
}

#[tokio::test]
async fn h3_channel_reuse_ring() {
    require_no_iwarp!();
    h3_channel_reuse(RingConfig::datagram()).await;
}
