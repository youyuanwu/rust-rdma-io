//! Tonic gRPC integration tests over RDMA streams.
//!
//! Tests verify that tonic server + client can communicate over RDMA
//! using `RdmaIncoming` (server) and `RdmaConnector` (client).
//!
//! Note: siw (soft-iWARP) can be flaky, particularly right after module
//! reload or when stale kernel resources remain from forcefully killed
//! processes. The test includes retry logic to tolerate transient failures.

use std::pin::Pin;
use std::time::Duration;

use rdma_io::async_stream::AsyncRdmaListener;
use rdma_io_tonic::{RdmaConnector, RdmaIncoming};

use greeter::greeter_client::GreeterClient;
use greeter::greeter_server::{Greeter, GreeterServer};
use greeter::{HelloReply, HelloRequest};
use tokio_stream::{Stream, StreamExt};
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status, Streaming};

pub mod greeter {
    tonic::include_proto!("greeter");
}

fn test_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    let sock = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    drop(sock);
    let bind_addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let connect_addr: std::net::SocketAddr = format!("{}:{port}", local_ip()).parse().unwrap();
    (bind_addr, connect_addr)
}

fn local_ip() -> String {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
    sock.connect("8.8.8.8:80").unwrap();
    sock.local_addr().unwrap().ip().to_string()
}

#[derive(Debug, Default)]
pub struct MyGreeter;

#[tonic::async_trait]
impl Greeter for MyGreeter {
    async fn say_hello(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<HelloReply>, Status> {
        let name = request.into_inner().name;
        Ok(Response::new(HelloReply {
            message: format!("Hello {}!", name),
        }))
    }

    type ServerStreamStream =
        Pin<Box<dyn Stream<Item = Result<HelloReply, Status>> + Send + 'static>>;

    async fn server_stream(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<Self::ServerStreamStream>, Status> {
        let name = request.into_inner().name;
        let stream = tokio_stream::iter(0..5).map(move |i| {
            Ok(HelloReply {
                message: format!("{name}-{i}"),
            })
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn client_stream(
        &self,
        request: Request<Streaming<HelloRequest>>,
    ) -> Result<Response<HelloReply>, Status> {
        let mut stream = request.into_inner();
        let mut names = Vec::new();
        while let Some(req) = stream.next().await {
            names.push(req?.name);
        }
        Ok(Response::new(HelloReply {
            message: format!("Hello {}!", names.join(", ")),
        }))
    }

    type BidiStreamStream =
        Pin<Box<dyn Stream<Item = Result<HelloReply, Status>> + Send + 'static>>;

    async fn bidi_stream(
        &self,
        request: Request<Streaming<HelloRequest>>,
    ) -> Result<Response<Self::BidiStreamStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(Ok(req)) = stream.next().await {
                let reply = HelloReply {
                    message: format!("echo: {}", req.name),
                };
                if tx.send(Ok(reply)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }
}

/// Start a tonic server over RDMA and return (client, shutdown_tx, server_handle).
async fn start_server_and_connect() -> (
    GreeterClient<Channel>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (bind_addr, connect_addr) = test_addrs();
    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
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

    tracing::info!("Server listening on {bind_addr}");

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
