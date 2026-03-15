//! Incoming RDMA connection stream for tonic server integration.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
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

/// Send-safe wrapper for a pointer to AsyncCmListener.
///
/// Safety: The listener is owned by RdmaIncoming and outlives the future.
/// AsyncCmListener is Send, so accessing it from another thread is safe.
struct ListenerPtr(*const AsyncCmListener);
unsafe impl Send for ListenerPtr {}

/// A [`Stream`] of incoming RDMA connections for use with
/// [`tonic::transport::Server::serve_with_incoming`].
///
/// Each accepted connection is wrapped as a [`TokioRdmaStream`] with the
/// required `tokio::io` and `Connected` trait implementations.
pub struct RdmaIncoming {
    listener: AsyncCmListener,
    buf_size: usize,
    accept_fut: Option<AcceptFut>,
}

// Safety: AsyncCmListener is Send, the boxed future is Send.
unsafe impl Send for RdmaIncoming {}

impl RdmaIncoming {
    /// Wrap an existing [`AsyncCmListener`] as an incoming stream.
    pub fn new(listener: AsyncCmListener) -> Self {
        Self::with_buf_size(listener, DEFAULT_BUF_SIZE)
    }

    /// Wrap an existing [`AsyncCmListener`] with a custom buffer size.
    pub fn with_buf_size(listener: AsyncCmListener, buf_size: usize) -> Self {
        Self {
            listener,
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
        Ok(Self::with_buf_size(listener, buf_size))
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

            // Safety: the listener is owned by RdmaIncoming. We only hold
            // one accept future at a time, and it is polled/dropped before
            // the listener. ListenerPtr makes the raw pointer Send-safe.
            let ptr = ListenerPtr(&this.listener as *const AsyncCmListener);
            let buf_size = this.buf_size;
            this.accept_fut = Some(Box::pin(accept_one(ptr, buf_size)));
        }
    }
}

/// Accept one connection from the listener behind a Send-safe pointer.
async fn accept_one(
    ptr: ListenerPtr,
    buf_size: usize,
) -> rdma_io::Result<AsyncRdmaStream<RdmaIoTransport>> {
    let listener = unsafe { &*ptr.0 };
    let config = TransportConfig {
        buf_size,
        ..TransportConfig::stream()
    };
    let transport = RdmaIoTransport::accept(listener, config).await?;
    Ok(AsyncRdmaStream::new(transport))
}
