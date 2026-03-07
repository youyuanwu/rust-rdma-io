// This crate's tests live in the `tests/` directory as integration tests.
// The build.rs generates protobuf code used by tonic_tests.rs.

/// Shared test helpers for RDMA integration tests.
pub mod test_helpers {
    use std::net::SocketAddr;

    /// Discover the first non-loopback IPv4 address (for siw0 over eth0).
    ///
    /// Uses the UDP connect trick: binding to 0.0.0.0 then "connecting" to an
    /// external address causes the kernel to pick the outgoing interface IP.
    pub fn local_ip() -> String {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
        sock.connect("8.8.8.8:80").unwrap();
        sock.local_addr().unwrap().ip().to_string()
    }

    /// Allocate a unique `(bind_addr, connect_addr)` pair using an ephemeral port.
    ///
    /// The bind address is `0.0.0.0:<port>` and the connect address is
    /// `<local_ip>:<port>`, which is necessary because siw on loopback
    /// (127.0.0.1) doesn't support `rdma_listen`.
    pub fn test_addrs() -> (SocketAddr, SocketAddr) {
        let sock = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        drop(sock);
        let bind_addr: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let connect_addr: SocketAddr = format!("{}:{port}", local_ip()).parse().unwrap();
        (bind_addr, connect_addr)
    }
}

/// Greeter gRPC service implementation for tonic integration tests.
///
/// Re-exports the generated proto types and provides `MyGreeter`,
/// a simple test service with unary, server-streaming, client-streaming,
/// and bidirectional-streaming RPCs.
pub mod greeter_service {
    pub mod greeter {
        tonic::include_proto!("greeter");
    }

    pub use greeter::greeter_client::GreeterClient;
    pub use greeter::greeter_server::{Greeter, GreeterServer};
    pub use greeter::{HelloReply, HelloRequest};

    use std::pin::Pin;
    use tokio_stream::{Stream, StreamExt};
    use tonic::{Request, Response, Status, Streaming};

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
                message: format!("Hello {name}!"),
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
}
