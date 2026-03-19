//! RDMA connector for tonic client integration.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use http::Uri;
use hyper_util::rt::TokioIo;
use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::transport::TransportBuilder;
use tower_service::Service;

use crate::stream::TokioRdmaStream;

/// A [`tower::Service<Uri>`] connector that establishes RDMA connections
/// for use with [`tonic::transport::Endpoint::connect_with_connector`].
///
/// Generic over the transport builder — use [`SendRecvConfig`] for Send/Recv
/// or [`CreditRingConfig`] for ring buffer transport.
///
/// [`SendRecvConfig`]: rdma_io::send_recv_transport::SendRecvConfig
/// [`CreditRingConfig`]: rdma_io::credit_ring_transport::CreditRingConfig
///
/// # Example
///
/// ```no_run
/// use rdma_io::send_recv_transport::SendRecvConfig;
/// use rdma_io_tonic::RdmaConnector;
/// use tonic::transport::Endpoint;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let connector = RdmaConnector::new(SendRecvConfig::stream());
/// let channel = Endpoint::from_static("http://10.0.0.1:50051")
///     .connect_with_connector(connector)
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct RdmaConnector<B: TransportBuilder> {
    builder: B,
}

impl<B: TransportBuilder> RdmaConnector<B> {
    /// Create a connector with the given transport builder.
    pub fn new(builder: B) -> Self {
        Self { builder }
    }
}

impl<B: TransportBuilder> Service<Uri> for RdmaConnector<B> {
    type Response = TokioIo<TokioRdmaStream<B::Transport>>;
    type Error = rdma_io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let builder = self.builder.clone();
        Box::pin(async move {
            let addr = uri_to_socket_addr(&uri)?;
            let transport = builder.connect(&addr).await?;
            let stream = AsyncRdmaStream::new(transport);
            let tokio_stream = TokioRdmaStream::new(stream);
            Ok(TokioIo::new(tokio_stream))
        })
    }
}

/// Extract a `SocketAddr` from a URI's authority (host:port).
pub(crate) fn uri_to_socket_addr(uri: &Uri) -> Result<SocketAddr, rdma_io::Error> {
    let host = uri
        .host()
        .ok_or_else(|| rdma_io::Error::InvalidArg("URI has no host".into()))?;
    let port = uri
        .port_u16()
        .ok_or_else(|| rdma_io::Error::InvalidArg("URI has no port".into()))?;
    // Strip brackets from IPv6 addresses (http::Uri includes them)
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = host
        .parse()
        .map_err(|e| rdma_io::Error::InvalidArg(format!("invalid host IP: {e}")))?;
    Ok(SocketAddr::new(ip, port))
}
