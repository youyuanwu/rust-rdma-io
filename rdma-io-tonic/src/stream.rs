//! RDMA stream with tokio::io traits and tonic `Connected` implementation.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use rdma_io::async_stream::AsyncRdmaStream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt};
use tonic::transport::server::Connected;

/// RDMA stream with `tokio::io` trait implementations.
///
/// Wraps [`AsyncRdmaStream`] via [`tokio_util::compat::Compat`] to bridge
/// from `futures_io` traits to `tokio::io` traits. Implements [`Connected`]
/// for tonic server integration.
pub struct TokioRdmaStream {
    inner: Compat<AsyncRdmaStream<rdma_io::rdma_transport::RdmaTransport>>,
    peer_addr: Option<SocketAddr>,
}

impl std::fmt::Debug for TokioRdmaStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioRdmaStream")
            .field("peer_addr", &self.peer_addr)
            .finish()
    }
}

impl TokioRdmaStream {
    /// Wrap an [`AsyncRdmaStream`] with tokio::io compatibility.
    pub fn new(stream: AsyncRdmaStream<rdma_io::rdma_transport::RdmaTransport>) -> Self {
        let peer_addr = stream.peer_addr();
        Self {
            inner: stream.compat(),
            peer_addr,
        }
    }

    /// Get the peer's socket address.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }
}

impl AsyncRead for TokioRdmaStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TokioRdmaStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Connection metadata for RDMA streams.
///
/// Accessible via `Request::extensions().get::<RdmaConnectInfo>()` in tonic
/// handlers.
#[derive(Clone, Debug)]
pub struct RdmaConnectInfo {
    /// Remote address of the RDMA peer (if available).
    pub remote_addr: Option<SocketAddr>,
}

impl Connected for TokioRdmaStream {
    type ConnectInfo = RdmaConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        RdmaConnectInfo {
            remote_addr: self.peer_addr,
        }
    }
}
