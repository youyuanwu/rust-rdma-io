//! tonic gRPC over HTTP/3 (QUIC) over RDMA tests.
//!
//! Full stack: tonic gRPC → HTTP/3 (h3) → QUIC (Quinn) → RDMA (rdma-io).
//! Uses tonic-h3 for the HTTP/3 gRPC layer and rdma-io-quinn's RdmaUdpSocket
//! as Quinn's transport.

use std::sync::Arc;

use quinn::{Endpoint, EndpointConfig};
use rdma_io_quinn::RdmaUdpSocket;

use rdma_io_tests::greeter_service::*;
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

/// Start a tonic-h3 gRPC server on an RDMA-backed Quinn endpoint.
/// Returns (server_endpoint, connect_addr, shutdown_tx, server_task).
async fn start_h3_server(
    certs: &[quinn::rustls::pki_types::CertificateDer<'static>],
    key: quinn::rustls::pki_types::PrivateKeyDer<'static>,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let server_config = make_h3_server_config(certs, key);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let server_socket =
        Arc::new(RdmaUdpSocket::bind(&test_helpers::bind_addr()).expect("server bind"));
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
async fn make_h3_client_endpoint(
    connect_addr: &std::net::SocketAddr,
    client_config: quinn::ClientConfig,
) -> Endpoint {
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let client_socket = RdmaUdpSocket::bind(&test_helpers::bind_addr()).expect("client bind");
    client_socket
        .connect_to(
            connect_addr,
            rdma_io::rdma_transport::TransportConfig::datagram(),
        )
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tonic_h3_unary_over_rdma() {
    let (certs, key) = generate_self_signed_cert();
    let client_config = make_h3_client_config(&certs);

    let (connect_addr, shutdown_tx, server_task) = start_h3_server(&certs, key).await;
    let client_endpoint = make_h3_client_endpoint(&connect_addr, client_config).await;
    let mut client = make_h3_grpc_client(&client_endpoint, &connect_addr);

    // Unary gRPC call
    let response = client
        .say_hello(tonic::Request::new(HelloRequest {
            name: "RDMA-H3".into(),
        }))
        .await
        .expect("unary RPC failed");

    let message = response.into_inner().message;
    println!("tonic_h3_unary: {message}");
    assert_eq!(message, "Hello RDMA-H3!");

    // Cleanup
    let _ = shutdown_tx.send(());
    client_endpoint.close(0u16.into(), b"done");
    server_task.await.expect("server task");
    println!("tonic_h3_unary_over_rdma passed!");
}

#[tokio::test]
async fn tonic_h3_server_stream_over_rdma() {
    let (certs, key) = generate_self_signed_cert();
    let client_config = make_h3_client_config(&certs);

    let (connect_addr, shutdown_tx, server_task) = start_h3_server(&certs, key).await;
    let client_endpoint = make_h3_client_endpoint(&connect_addr, client_config).await;
    let mut client = make_h3_grpc_client(&client_endpoint, &connect_addr);

    // Server-streaming gRPC call
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

    println!("tonic_h3_server_stream: {messages:?}");
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0], "stream-0");
    assert_eq!(messages[4], "stream-4");

    // Cleanup
    let _ = shutdown_tx.send(());
    client_endpoint.close(0u16.into(), b"done");
    server_task.await.expect("server task");
    println!("tonic_h3_server_stream_over_rdma passed!");
}
