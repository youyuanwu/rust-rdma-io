//! Quinn QUIC echo test over RDMA.
//!
//! Each test has a generic body and two concrete entry points:
//! `_default` (Send/Recv) and `_ring` (Ring buffer, skipped on iWARP).

use std::sync::Arc;

use quinn::{ClientConfig, Endpoint, ServerConfig};
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport::TransportBuilder;
use rdma_io_quinn::RdmaUdpSocket;

use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers;

/// Retry `connect_to` on EADDRINUSE (siw async port release).
async fn connect_with_retry<B: TransportBuilder>(
    socket: &RdmaUdpSocket<B>,
    addr: &std::net::SocketAddr,
    label: &str,
) {
    for attempt in 0u64..5 {
        match socket.connect_to(addr).await {
            Ok(()) => return,
            Err(e) => {
                let is_addr_in_use =
                    matches!(&e, rdma_io::Error::Verbs(io) if io.raw_os_error() == Some(98));
                if is_addr_in_use && attempt < 4 {
                    eprintln!("{label} connect attempt {attempt} EADDRINUSE, retrying...");
                    tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
                    continue;
                }
                panic!("{label} pre-connect failed: {e}");
            }
        }
    }
}

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
    connect_with_retry(&client_socket, &connect_addr, "client").await;
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
    quinn_echo(SendRecvConfig::datagram()).await;
}

#[tokio::test]
async fn quinn_echo_ring() {
    require_no_iwarp!();
    quinn_echo(CreditRingConfig::datagram()).await;
}

// ===========================================================================
// quinn_multi_peer — two clients echo concurrently via the same server.
// ===========================================================================

async fn quinn_multi_peer<B: TransportBuilder>(builder: B) {
    let (certs, key) = generate_self_signed_cert();
    let server_config = make_server_config(&certs, key);
    let client_config = make_client_config(&certs);
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);

    // Server
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

    // Server task: accept 2 connections, echo on each.
    let server = tokio::spawn(async move {
        for i in 0..2 {
            let incoming = server_endpoint.accept().await.expect("no incoming");
            let connection = incoming.await.expect("accept failed");
            println!(
                "Server: accepted peer-{i} from {}",
                connection.remote_address()
            );
            tokio::spawn(async move {
                let (mut send, mut recv) = connection.accept_bi().await.expect("accept_bi");
                let data = recv.read_to_end(1024).await.expect("read");
                send.write_all(&data).await.expect("write");
                send.finish().expect("finish");
                send.stopped().await.ok();
            });
        }
    });

    // Client 1
    let client_socket_1 =
        RdmaUdpSocket::bind(&test_helpers::bind_addr(), builder.clone()).expect("client1 bind");
    connect_with_retry(&client_socket_1, &connect_addr, "client1").await;

    let mut ep1 = Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        None,
        Arc::new(client_socket_1),
        runtime.clone(),
    )
    .expect("client1 endpoint");
    ep1.set_default_client_config(client_config.clone());

    // Client 2
    let client_socket_2 =
        RdmaUdpSocket::bind(&test_helpers::bind_addr(), builder).expect("client2 bind");
    connect_with_retry(&client_socket_2, &connect_addr, "client2").await;

    let mut ep2 = Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        None,
        Arc::new(client_socket_2),
        runtime,
    )
    .expect("client2 endpoint");
    ep2.set_default_client_config(client_config);

    // Both clients connect and echo concurrently.
    let (r1, r2) = tokio::join!(
        async {
            let conn = ep1
                .connect(connect_addr, "localhost")
                .unwrap()
                .await
                .expect("connect1");
            let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
            send.write_all(b"peer-1").await.expect("write");
            send.finish().expect("finish");
            recv.read_to_end(1024).await.expect("read")
        },
        async {
            let conn = ep2
                .connect(connect_addr, "localhost")
                .unwrap()
                .await
                .expect("connect2");
            let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
            send.write_all(b"peer-2").await.expect("write");
            send.finish().expect("finish");
            recv.read_to_end(1024).await.expect("read")
        },
    );

    assert_eq!(r1, b"peer-1");
    assert_eq!(r2, b"peer-2");
    println!("Multi-peer echo verified!");

    server.await.expect("server task");
}

#[tokio::test]
async fn quinn_multi_peer_default() {
    quinn_multi_peer(SendRecvConfig::datagram()).await;
}

#[tokio::test]
async fn quinn_multi_peer_ring() {
    require_no_iwarp!();
    quinn_multi_peer(CreditRingConfig::datagram()).await;
}
