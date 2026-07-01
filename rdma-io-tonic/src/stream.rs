//! RDMA stream with tokio::io traits and tonic `Connected` implementation.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::transport::Transport;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::Connected;

/// RDMA stream with `tokio::io` trait implementations.
///
/// Wraps [`AsyncRdmaStream<T>`], which implements the `tokio::io` traits
/// natively (via `rdma-io`'s `tokio` feature). Implements [`Connected`] for
/// tonic server integration.
pub struct TokioRdmaStream<T: Transport> {
    inner: AsyncRdmaStream<T>,
    peer_addr: Option<SocketAddr>,
}

impl<T: Transport> std::fmt::Debug for TokioRdmaStream<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioRdmaStream")
            .field("peer_addr", &self.peer_addr)
            .finish()
    }
}

impl<T: Transport> TokioRdmaStream<T> {
    /// Wrap an [`AsyncRdmaStream`] with tokio::io compatibility.
    pub fn new(stream: AsyncRdmaStream<T>) -> Self {
        let peer_addr = stream.peer_addr();
        Self {
            inner: stream,
            peer_addr,
        }
    }

    /// Get the peer's socket address.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }
}

impl<T: Transport> AsyncRead for TokioRdmaStream<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<T: Transport> AsyncWrite for TokioRdmaStream<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn is_write_vectored(&self) -> bool {
        // Advertise vectored writes so hyper/h2 hand us gathered frame slices
        // (header + payload) in one call; the inner stream coalesces them into
        // a single RDMA send.
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
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

impl<T: Transport> Connected for TokioRdmaStream<T> {
    type ConnectInfo = RdmaConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        RdmaConnectInfo {
            remote_addr: self.peer_addr,
        }
    }
}
