//! Quinn QUIC echo test over RDMA.
//!
//! Each test has a generic body and two concrete entry points:
//! `_default` (Send/Recv) and `_ring` (Ring buffer, skipped on iWARP).

use std::sync::Arc;

use quinn::{ClientConfig, Endpoint, ServerConfig};
use rdma_io::rdma_ring_transport::RingConfig;
use rdma_io::rdma_transport::TransportConfig;
use rdma_io::transport::TransportBuilder;
use rdma_io_quinn::RdmaUdpSocket;

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

fn make_server_config(
    certs: &[quinn::rustls::pki_types::CertificateDer<'static>],
    key: quinn::rustls::pki_types::PrivateKeyDer<'static>,
) -> ServerConfig {
    ServerConfig::with_single_cert(certs.to_vec(), key).unwrap()
}

fn make_client_config(
    server_certs: &[quinn::rustls::pki_types::CertificateDer<'static>],
) -> ClientConfig {
    let mut roots = quinn::rustls::RootCertStore::empty();
    for cert in server_certs {
        roots.add(cert.clone()).unwrap();
    }
    let crypto = quinn::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
    ))
}

// ===========================================================================
// quinn_echo — QUIC bidirectional stream echo over RDMA.
// ===========================================================================

async fn quinn_echo<B: TransportBuilder>(builder: B) {
    let (certs, key) = generate_self_signed_cert();
    let server_config = make_server_config(&certs, key);
    let client_config = make_client_config(&certs);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    let server_socket =
        RdmaUdpSocket::bind(&test_helpers::bind_addr(), builder.clone()).expect("server bind");
    let connect_addr = test_helpers::connect_addr_for(Some(server_socket.bound_addr()));

    let server_endpoint = Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_config),
        Arc::new(server_socket),
        runtime.clone(),
    )
    .expect("server endpoint");

    let server = tokio::spawn(async move {
        let incoming = server_endpoint.accept().await.expect("no incoming");
        let connection = incoming.await.expect("accept failed");
        println!("Server: accepted from {}", connection.remote_address());

        let (mut send, mut recv) = connection.accept_bi().await.expect("accept_bi");
        let data = recv.read_to_end(1024).await.expect("read");
        println!(
            "Server: received {} bytes: {:?}",
            data.len(),
            String::from_utf8_lossy(&data)
        );
        send.write_all(&data).await.expect("write");
        send.finish().expect("finish");
        send.stopped().await.ok();
    });

    let client_socket =
        RdmaUdpSocket::bind(&test_helpers::bind_addr(), builder).expect("client bind");
    println!("Client: pre-connecting RDMA to {connect_addr}");
    client_socket
        .connect_to(&connect_addr)
        .await
        .expect("RDMA pre-connect");
    println!("Client: RDMA connection established");

    let mut client_endpoint = Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        None,
        Arc::new(client_socket),
        runtime,
    )
    .expect("client endpoint");
    client_endpoint.set_default_client_config(client_config);

    println!("Client: QUIC connecting to {connect_addr}");
    let connection = client_endpoint
        .connect(connect_addr, "localhost")
        .expect("connect call")
        .await
        .expect("connect");
    println!("Client: connected to {}", connection.remote_address());

    let (mut send, mut recv) = connection.open_bi().await.expect("open_bi");
    send.write_all(b"hello rdma").await.expect("write");
    send.finish().expect("finish");

    let response = recv.read_to_end(1024).await.expect("read");
    assert_eq!(response, b"hello rdma");
    println!("Client: echo verified!");

    server.await.expect("server task");
}

#[tokio::test]
async fn quinn_echo_default() {
    quinn_echo(TransportConfig::datagram()).await;
}

#[tokio::test]
async fn quinn_echo_ring() {
    require_no_iwarp!();
    quinn_echo(RingConfig::datagram()).await;
}
