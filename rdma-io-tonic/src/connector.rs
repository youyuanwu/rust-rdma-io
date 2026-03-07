//! RDMA connector for tonic client integration.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use http::Uri;
use hyper_util::rt::TokioIo;
use rdma_io::async_stream::AsyncRdmaStream;
use tower_service::Service;

use crate::stream::TokioRdmaStream;

/// Default buffer size (matches AsyncRdmaStream default).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

/// A [`tower::Service<Uri>`] connector that establishes RDMA connections
/// for use with [`tonic::transport::Endpoint::connect_with_connector`].
///
/// # Example
///
/// ```no_run
/// use rdma_io_tonic::RdmaConnector;
/// use tonic::transport::Endpoint;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let connector = RdmaConnector::new();
/// let channel = Endpoint::from_static("http://10.0.0.1:50051")
///     .connect_with_connector(connector)
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct RdmaConnector {
    buf_size: usize,
}

impl RdmaConnector {
    /// Create a connector with the default buffer size (64 KiB).
    pub fn new() -> Self {
        Self {
            buf_size: DEFAULT_BUF_SIZE,
        }
    }

    /// Create a connector with a custom buffer size.
    pub fn with_buf_size(buf_size: usize) -> Self {
        Self { buf_size }
    }
}

impl Default for RdmaConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Service<Uri> for RdmaConnector {
    type Response = TokioIo<TokioRdmaStream>;
    type Error = rdma_io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let buf_size = self.buf_size;
        Box::pin(async move {
            let addr = uri_to_socket_addr(&uri)?;
            let stream = AsyncRdmaStream::connect_with_buf_size(&addr, buf_size).await?;
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
