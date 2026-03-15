//! Incoming RDMA connection stream for tonic server integration.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::rdma_transport::{RdmaTransport as RdmaIoTransport, TransportConfig};

use crate::stream::TokioRdmaStream;

/// Default buffer size (64 KiB).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

type AcceptFut =
    Pin<Box<dyn Future<Output = rdma_io::Result<AsyncRdmaStream<RdmaIoTransport>>> + Send>>;

/// A [`Stream`] of incoming RDMA connections for use with
/// [`tonic::transport::Server::serve_with_incoming`].
///
/// Each accepted connection is wrapped as a [`TokioRdmaStream`] with the
/// required `tokio::io` and `Connected` trait implementations.
pub struct RdmaIncoming {
    listener: Arc<AsyncCmListener>,
    buf_size: usize,
    accept_fut: Option<AcceptFut>,
}

impl RdmaIncoming {
    /// Wrap an existing [`AsyncCmListener`] as an incoming stream.
    pub fn new(listener: AsyncCmListener) -> Self {
        Self::with_buf_size(listener, DEFAULT_BUF_SIZE)
    }

    /// Wrap an existing [`AsyncCmListener`] with a custom buffer size.
    pub fn with_buf_size(listener: AsyncCmListener, buf_size: usize) -> Self {
        Self {
            listener: Arc::new(listener),
            buf_size,
            accept_fut: None,
        }
    }

    /// Bind to a local address and create an incoming RDMA connection stream.
    pub fn bind(addr: &SocketAddr) -> rdma_io::Result<Self> {
        Self::bind_with_buf_size(addr, DEFAULT_BUF_SIZE)
    }

    /// Bind with a custom buffer size for accepted streams.
    pub fn bind_with_buf_size(addr: &SocketAddr, buf_size: usize) -> rdma_io::Result<Self> {
        let listener = AsyncCmListener::bind(addr)?;
        Ok(Self {
            listener: Arc::new(listener),
            buf_size,
            accept_fut: None,
        })
    }

    /// Get the local socket address the listener is bound to.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr()
    }
}

impl Stream for RdmaIncoming {
    type Item = Result<TokioRdmaStream, rdma_io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(fut) = this.accept_fut.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(stream)) => {
                        this.accept_fut = None;
                        return Poll::Ready(Some(Ok(TokioRdmaStream::new(stream))));
                    }
                    Poll::Ready(Err(e)) => {
                        this.accept_fut = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                }
            }

            let listener = Arc::clone(&this.listener);
            let buf_size = this.buf_size;
            this.accept_fut = Some(Box::pin(accept_one(listener, buf_size)));
        }
    }
}

/// Accept one connection from the listener.
async fn accept_one(
    listener: Arc<AsyncCmListener>,
    buf_size: usize,
) -> rdma_io::Result<AsyncRdmaStream<RdmaIoTransport>> {
    let config = TransportConfig {
        buf_size,
        ..TransportConfig::stream()
    };
    let transport = RdmaIoTransport::accept(&listener, config).await?;
    Ok(AsyncRdmaStream::new(transport))
}
