//! Tonic gRPC integration tests over RDMA streams.
//!
//! Tests verify that tonic server + client can communicate over RDMA
//! using `RdmaIncoming` (server) and `RdmaConnector` (client).

use std::time::Duration;

use rdma_io::async_cm::AsyncCmListener;
use rdma_io_tonic::{RdmaConnector, RdmaIncoming};

use rdma_io_tests::greeter_service::*;
use rdma_io_tests::test_helpers::{bind_addr, connect_addr_for};

use tokio_stream::StreamExt;
use tonic::Request;
use tonic::transport::{Channel, Endpoint, Server};

/// Start a tonic server over RDMA and return (client, shutdown_tx, server_handle).
async fn start_server_and_connect() -> (
    GreeterClient<Channel>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = AsyncCmListener::bind(&bind_addr()).unwrap();
    let connect_addr = connect_addr_for(listener.local_addr());
    let incoming = RdmaIncoming::new(listener);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(GreeterServer::new(MyGreeter))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    tracing::info!("Server listening on {connect_addr}");

    tokio::time::sleep(Duration::from_millis(200)).await;

    tracing::info!("Client connecting to {connect_addr}...");
    let connector = RdmaConnector::default();
    let uri = format!("http://{}:{}", connect_addr.ip(), connect_addr.port());
    let mut channel = None;
    for attempt in 1..=3 {
        match Endpoint::from_shared(uri.clone())
            .unwrap()
            .connect_with_connector(connector.clone())
            .await
        {
            Ok(ch) => {
                channel = Some(ch);
                break;
            }
            Err(e) if attempt < 3 => {
                tracing::warn!("connect attempt {attempt} failed: {e}, retrying...");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("RDMA connect failed after {attempt} attempts: {e}"),
        }
    }
    let client = GreeterClient::new(channel.unwrap());
    tracing::info!("Client connected");
    (client, shutdown_tx, server_handle)
}

/// Test: unary RPC over RDMA.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn tonic_greeter_over_rdma() {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect().await;

    tracing::info!("Calling SayHello (unary)...");
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.say_hello(Request::new(HelloRequest {
            name: "RDMA".into(),
        })),
    )
    .await
    .expect("gRPC call timed out")
    .expect("gRPC call failed");

    assert_eq!(response.into_inner().message, "Hello RDMA!");
    tracing::info!("Unary call succeeded");

    drop(client);
    shutdown_tx.send(()).unwrap();
    server_handle.await.unwrap();
    tracing::info!("Server shut down");
}

/// Test: server streaming RPC over RDMA — server sends 5 replies.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn tonic_server_stream_over_rdma() {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect().await;

    tracing::info!("Calling ServerStream...");
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.server_stream(Request::new(HelloRequest {
            name: "stream".into(),
        })),
    )
    .await
    .expect("gRPC call timed out")
    .expect("gRPC call failed");

    let mut stream = response.into_inner();
    let mut messages = Vec::new();
    while let Some(reply) = stream.next().await {
        messages.push(reply.expect("stream error").message);
    }
    tracing::info!("Received {} server-stream replies", messages.len());
    assert_eq!(
        messages,
        vec!["stream-0", "stream-1", "stream-2", "stream-3", "stream-4"]
    );

    drop(client);
    shutdown_tx.send(()).unwrap();
    server_handle.await.unwrap();
    tracing::info!("Server shut down");
}

/// Test: client streaming RPC over RDMA — client sends 3 names, server aggregates.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn tonic_client_stream_over_rdma() {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect().await;

    tracing::info!("Calling ClientStream with 3 requests...");
    let request_stream =
        tokio_stream::iter(
            ["Alice", "Bob", "Charlie"]
                .into_iter()
                .map(|n| HelloRequest {
                    name: n.to_string(),
                }),
        );

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.client_stream(Request::new(request_stream)),
    )
    .await
    .expect("gRPC call timed out")
    .expect("gRPC call failed");

    let msg = response.into_inner().message;
    tracing::info!("ClientStream response: {msg}");
    assert_eq!(msg, "Hello Alice, Bob, Charlie!");

    drop(client);
    shutdown_tx.send(()).unwrap();
    server_handle.await.unwrap();
    tracing::info!("Server shut down");
}

/// Test: bidirectional streaming RPC over RDMA — client sends, server echoes each.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn tonic_bidi_stream_over_rdma() {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect().await;

    tracing::info!("Calling BidiStream with 3 requests...");
    let request_stream =
        tokio_stream::iter(["one", "two", "three"].into_iter().map(|n| HelloRequest {
            name: n.to_string(),
        }));

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.bidi_stream(Request::new(request_stream)),
    )
    .await
    .expect("gRPC call timed out")
    .expect("gRPC call failed");

    let mut stream = response.into_inner();
    let mut messages = Vec::new();
    while let Some(reply) = stream.next().await {
        messages.push(reply.expect("stream error").message);
    }
    tracing::info!("Received {} bidi-stream replies", messages.len());
    assert_eq!(messages, vec!["echo: one", "echo: two", "echo: three"]);

    drop(client);
    shutdown_tx.send(()).unwrap();
    server_handle.await.unwrap();
    tracing::info!("Server shut down");
}
