//! Tonic gRPC integration tests over RDMA streams.
//!
//! Tests verify that tonic server + client can communicate over RDMA
//! using `RdmaIncoming` (server) and `RdmaConnector` (client).
//!
//! Note: siw (soft-iWARP) can be flaky, particularly right after module
//! reload or when stale kernel resources remain from forcefully killed
//! processes. The test includes retry logic to tolerate transient failures.

use std::time::Duration;

use rdma_io::async_stream::AsyncRdmaListener;
use rdma_io_tonic::{RdmaConnector, RdmaIncoming};

use greeter::greeter_client::GreeterClient;
use greeter::greeter_server::{Greeter, GreeterServer};
use greeter::{HelloReply, HelloRequest};
use tonic::transport::{Endpoint, Server};
use tonic::{Request, Response, Status};

pub mod greeter {
    tonic::include_proto!("greeter");
}

fn test_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    // Bind to port 0 to let the OS pick a free port, then extract it.
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
        let reply = HelloReply {
            message: format!("Hello {}!", name),
        };
        Ok(Response::new(reply))
    }
}

/// Test: tonic Greeter service over RDMA — server uses RdmaIncoming, client uses RdmaConnector.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn tonic_greeter_over_rdma() {
    let (bind_addr, connect_addr) = test_addrs();

    // Start RDMA listener and wrap as tonic incoming stream
    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
    let incoming = RdmaIncoming::new(listener);

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn tonic server with graceful shutdown
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(GreeterServer::new(MyGreeter))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    // Give server time to start listening
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect with retry — siw can reject connections transiently after
    // module reload or when stale resources linger in the kernel.
    tracing::info!("Client connecting to server at {connect_addr}...");
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
    let channel = channel.unwrap();
    let mut client = GreeterClient::new(channel);

    tracing::info!("Client connected, making gRPC call...");
    // Make the gRPC call with a timeout to avoid indefinite hangs
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

    tracing::info!("gRPC call succeeded, shutting down server...");
    // Graceful shutdown: drop client, signal server to stop, wait for drain.
    drop(client);
    shutdown_tx.send(()).unwrap();
    tracing::info!("Waiting for server to shut down...");
    server_handle.await.unwrap();
    tracing::info!("Server shut down, test complete.");
}
