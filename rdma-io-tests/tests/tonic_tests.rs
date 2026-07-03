//! Tonic gRPC integration tests over RDMA streams.
//!
//! Each test has a generic body (`test_name<B: TransportBuilder>`) and two
//! concrete entry points: `test_name_default` (Send/Recv) and `test_name_ring`
//! (Ring buffer). The ring variant is skipped on iWARP.

use std::fmt::Debug;
use std::time::Duration;

use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport::TransportBuilder;
use rdma_io_tonic::{RdmaConnector, RdmaIncoming};

use rdma_io_tests::greeter_service::*;
use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::connect_addr_for;

use tokio_stream::StreamExt;
use tonic::Request;
use tonic::transport::{Channel, Endpoint, Server};

// ---------------------------------------------------------------------------
// Generic helper
// ---------------------------------------------------------------------------

/// Start a tonic server over RDMA and return (client, shutdown_tx, server_handle).
async fn start_server_and_connect<B: TransportBuilder + Debug>(
    builder: B,
) -> (
    GreeterClient<Channel>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());
    let incoming = RdmaIncoming::new(listener, builder.clone());

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
    let connector = RdmaConnector::new(builder);
    let uri = format!("http://{}:{}", connect_addr.ip(), connect_addr.port());
    let mut channel = None;
    for attempt in 1..=5 {
        match Endpoint::from_shared(uri.clone())
            .unwrap()
            .connect_with_connector(connector.clone())
            .await
        {
            Ok(ch) => {
                channel = Some(ch);
                break;
            }
            Err(e) if attempt < 5 => {
                tracing::warn!("connect attempt {attempt} failed: {e}, retrying...");
                tokio::time::sleep(Duration::from_millis(100 * attempt)).await;
            }
            Err(e) => panic!("RDMA connect failed after {attempt} attempts: {e}"),
        }
    }
    let client = GreeterClient::new(channel.unwrap());
    tracing::info!("Client connected");
    (client, shutdown_tx, server_handle)
}

// ===========================================================================
// greeter — unary RPC over RDMA.
// ===========================================================================

async fn greeter<B: TransportBuilder + Debug>(builder: B) {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect(builder).await;

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
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn greeter_default() {
    greeter(SendRecvConfig::stream()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn greeter_ring() {
    require_no_iwarp!();
    greeter(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn greeter_read_ring() {
    require_no_iwarp!();
    greeter(ReadRingConfig::default()).await;
}

// ===========================================================================
// server_stream — server sends 5 replies.
// ===========================================================================

async fn server_stream<B: TransportBuilder + Debug>(builder: B) {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect(builder).await;

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
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn server_stream_default() {
    server_stream(SendRecvConfig::stream()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn server_stream_ring() {
    require_no_iwarp!();
    server_stream(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn server_stream_read_ring() {
    require_no_iwarp!();
    server_stream(ReadRingConfig::default()).await;
}

// ===========================================================================
// client_stream — client sends 3 names, server aggregates.
// ===========================================================================

async fn client_stream<B: TransportBuilder + Debug>(builder: B) {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect(builder).await;

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
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn client_stream_default() {
    client_stream(SendRecvConfig::stream()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn client_stream_ring() {
    require_no_iwarp!();
    client_stream(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn client_stream_read_ring() {
    require_no_iwarp!();
    client_stream(ReadRingConfig::default()).await;
}

// ===========================================================================
// bidi_stream — client sends, server echoes each.
// ===========================================================================

async fn bidi_stream<B: TransportBuilder + Debug>(builder: B) {
    let (mut client, shutdown_tx, server_handle) = start_server_and_connect(builder).await;

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
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn bidi_stream_default() {
    bidi_stream(SendRecvConfig::stream()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn bidi_stream_ring() {
    require_no_iwarp!();
    bidi_stream(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn bidi_stream_read_ring() {
    require_no_iwarp!();
    bidi_stream(ReadRingConfig::default()).await;
}

// ===========================================================================
// concurrent_unary_load — N concurrent in-flight unary RPCs multiplexed over a
// single h2 connection, sustained over several rounds. Mirrors the bench tool's
// `--in-flight` knob (concurrent RPCs per connection) at unit-test scale, and
// is the regression guard for the concurrent-RPC gRPC-over-RDMA path the
// read-ring deadlock lived in (docs/bugs/read-ring-concurrent-stream-deadlock.md).
// ===========================================================================

async fn concurrent_unary_load<B: TransportBuilder + Debug>(builder: B) {
    let (client, shutdown_tx, server_handle) = start_server_and_connect(builder).await;

    const IN_FLIGHT: usize = 8;
    const ROUNDS: usize = 8;
    // Small payload mirrors the deadlock repro (64 B); embedded in the name so
    // the echoed response is checkable.
    let payload = "x".repeat(64);

    for round in 0..ROUNDS {
        let mut handles = Vec::with_capacity(IN_FLIGHT);
        for i in 0..IN_FLIGHT {
            let mut c = client.clone(); // clones share the one h2 connection
            let name = format!("{payload}-{round}-{i}");
            handles.push(tokio::spawn(async move {
                let resp = tokio::time::timeout(
                    Duration::from_secs(20),
                    c.say_hello(Request::new(HelloRequest { name: name.clone() })),
                )
                .await
                .expect("concurrent gRPC call timed out")
                .expect("concurrent gRPC call failed");
                assert_eq!(resp.into_inner().message, format!("Hello {name}!"));
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    drop(client);
    shutdown_tx.send(()).unwrap();
    server_handle.await.unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn concurrent_unary_load_default() {
    concurrent_unary_load(SendRecvConfig::stream_with_depth(8)).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn concurrent_unary_load_ring() {
    require_no_iwarp!();
    concurrent_unary_load(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn concurrent_unary_load_read_ring() {
    require_no_iwarp!();
    concurrent_unary_load(ReadRingConfig::default()).await;
}
