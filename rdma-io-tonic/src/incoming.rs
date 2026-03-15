//! Incoming RDMA connection stream for tonic server integration.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
use rdma_io::async_cm::AsyncCmListener;
use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::transport::TransportBuilder;

use crate::stream::TokioRdmaStream;

/// A [`Stream`] of incoming RDMA connections for use with
/// [`tonic::transport::Server::serve_with_incoming`].
///
/// Generic over the transport builder — use [`TransportConfig`] for Send/Recv
/// or [`RingConfig`] for ring buffer transport.
///
/// [`TransportConfig`]: rdma_io::rdma_transport::TransportConfig
/// [`RingConfig`]: rdma_io::rdma_ring_transport::RingConfig
///
/// # Example
///
/// ```no_run
/// use rdma_io::rdma_transport::TransportConfig;
/// use rdma_io_tonic::RdmaIncoming;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let incoming = RdmaIncoming::bind(
///     &"0.0.0.0:50051".parse().unwrap(),
///     TransportConfig::stream(),
/// )?;
/// // Server::builder().add_service(svc).serve_with_incoming(incoming).await?;
/// # Ok(())
/// # }
/// ```
pub struct RdmaIncoming<B: TransportBuilder> {
    listener: Arc<AsyncCmListener>,
    builder: B,
    accept_fut: Option<AcceptFut<B::Transport>>,
}

type AcceptFut<T> = Pin<Box<dyn Future<Output = rdma_io::Result<AsyncRdmaStream<T>>> + Send>>;

impl<B: TransportBuilder> RdmaIncoming<B> {
    /// Wrap an existing [`AsyncCmListener`] with a transport builder.
    pub fn new(listener: AsyncCmListener, builder: B) -> Self {
        Self {
            listener: Arc::new(listener),
            builder,
            accept_fut: None,
        }
    }

    /// Bind to a local address and create an incoming RDMA connection stream.
    pub fn bind(addr: &SocketAddr, builder: B) -> rdma_io::Result<Self> {
        let listener = AsyncCmListener::bind(addr)?;
        Ok(Self {
            listener: Arc::new(listener),
            builder,
            accept_fut: None,
        })
    }

    /// Get the local socket address the listener is bound to.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr()
    }
}

impl<B: TransportBuilder> Stream for RdmaIncoming<B> {
    type Item = Result<TokioRdmaStream<B::Transport>, rdma_io::Error>;

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
            let builder = this.builder.clone();
            this.accept_fut = Some(Box::pin(accept_one(listener, builder)));
        }
    }
}

/// Accept one connection from the listener using the given builder.
async fn accept_one<B: TransportBuilder>(
    listener: Arc<AsyncCmListener>,
    builder: B,
) -> rdma_io::Result<AsyncRdmaStream<B::Transport>> {
    let transport = builder.accept(&listener).await?;
    Ok(AsyncRdmaStream::new(transport))
}
