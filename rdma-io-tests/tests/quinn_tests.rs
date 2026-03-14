//! Quinn QUIC echo test over RDMA.

use std::sync::Arc;

use quinn::{ClientConfig, Endpoint, ServerConfig};
use rdma_io_quinn::RdmaUdpSocket;

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

fn make_server_config() -> ServerConfig {
    let (certs, key) = generate_self_signed_cert();
    ServerConfig::with_single_cert(certs, key).unwrap()
}

fn make_client_config() -> ClientConfig {
    let _roots = quinn::rustls::RootCertStore::empty();
    // For testing: accept any cert
    let crypto = quinn::rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerify))
        .with_no_client_auth();
    ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
    ))
}

#[derive(Debug)]
struct SkipVerify;

impl quinn::rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &[quinn::rustls::pki_types::CertificateDer<'_>],
        _: &quinn::rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: quinn::rustls::pki_types::UnixTime,
    ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error> {
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        quinn::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

use rdma_io_tests::test_helpers;

#[tokio::test]
async fn quinn_echo_over_rdma() {
    let server_config = make_server_config();
    let client_config = make_client_config();
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    // 1. Bind server socket + create server Quinn endpoint FIRST
    //    (so poll_accept can handle the incoming RDMA connection)
    let server_socket = RdmaUdpSocket::bind(&test_helpers::bind_addr()).expect("server bind");
    let connect_addr = test_helpers::connect_addr_for(Some(server_socket.bound_addr()));

    let server_endpoint = Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_config),
        Arc::new(server_socket),
        runtime.clone(),
    )
    .expect("server endpoint");

    // 2. Server accept task — spawned before RDMA connect so the
    //    server's poll_recv → poll_accept can handle the RDMA CM handshake.
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
        // Keep connection alive until client reads the echo response.
        // Dropping connection immediately causes ApplicationClosed.
        send.stopped().await.ok();
    });

    // 3. Bind client socket and pre-establish RDMA connection.
    //    The server's poll_accept (driven by Quinn's EndpointDriver) will
    //    handle the accept side concurrently.
    let client_socket = RdmaUdpSocket::bind(&test_helpers::bind_addr()).expect("client bind");
    println!("Client: pre-connecting RDMA to {connect_addr}");
    client_socket
        .connect_to(
            &connect_addr,
            rdma_io::rdma_transport::TransportConfig::datagram(),
        )
        .await
        .expect("RDMA pre-connect");
    println!("Client: RDMA connection established");

    // 4. Create client Quinn endpoint and do QUIC handshake
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
