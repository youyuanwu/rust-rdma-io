//! Async RDMA Stream — async `read` + `write` over a [`Transport`].
//!
//! Provides TCP-like async semantics over an RDMA transport, built on
//! the [`Transport`](crate::transport::Transport) trait for completion-driven I/O.
//!
//! # Architecture
//!
//! `AsyncRdmaStream<T>` is generic over `T: Transport`. The default
//! concrete type is [`RdmaTransport`](crate::rdma_transport::RdmaTransport)
//! which uses RDMA Send/Recv verbs. Convenience constructors
//! (`connect`, `connect_with_buf_size`) hide the generic parameter.
//!
//! # Protocol
//!
//! Pure RDMA SEND/RECV with no application-level framing. Each `write()`
//! becomes one transport send; each `read()` consumes one recv completion.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};

use crate::async_cm::AsyncCmListener;
use crate::rdma_transport::{RdmaTransport, TransportConfig};
use crate::transport::{RecvCompletion, Transport};

/// Default buffer size for stream send/recv (64 KiB).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

/// An async RDMA stream with `read` and `write` methods.
///
/// Generic over `T: Transport` for testability and future transport variants.
/// Created via [`AsyncRdmaStream::connect`] (client) or
/// [`AsyncRdmaListener::accept`] (server).
///
/// # Example
///
/// ```no_run
/// use rdma_io::async_stream::{AsyncRdmaListener, AsyncRdmaStream};
///
/// # async fn example() -> rdma_io::Result<()> {
/// // Server
/// let listener = AsyncRdmaListener::bind(&"0.0.0.0:9999".parse().unwrap())?;
/// let mut stream = listener.accept().await?;
/// let mut buf = [0u8; 1024];
/// let n = stream.read(&mut buf).await?;
///
/// // Client
/// let mut stream = AsyncRdmaStream::connect(&"10.0.0.1:9999".parse().unwrap()).await?;
/// stream.write(b"hello").await?;
/// # Ok(())
/// # }
/// ```
pub struct AsyncRdmaStream<T: Transport = RdmaTransport> {
    transport: T,
    /// Partially consumed recv: (buf_index, offset, total_len).
    recv_pending: Option<(usize, usize, usize)>,
    /// In-flight send length. None if send slot is free.
    write_pending: Option<usize>,
    /// Set when transport returns Err from poll_recv (QP entered ERROR state).
    /// Once set, poll_read always returns Ok(0) — the QP will never
    /// produce another recv completion.
    eof: bool,
}

// T: Transport is Send + Sync, and our own fields are trivially Unpin/Send/Sync.
impl<T: Transport> Unpin for AsyncRdmaStream<T> {}

// Safety: T: Send + Sync (required by Transport), remaining fields are trivially Send + Sync.
unsafe impl<T: Transport> Send for AsyncRdmaStream<T> {}
unsafe impl<T: Transport> Sync for AsyncRdmaStream<T> {}

impl<T: Transport> fmt::Debug for AsyncRdmaStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncRdmaStream")
            .field("local_addr", &self.transport.local_addr())
            .field("peer_addr", &self.transport.peer_addr())
            .field("eof", &self.eof)
            .field("recv_pending", &self.recv_pending.is_some())
            .field("write_pending", &self.write_pending.is_some())
            .finish()
    }
}

impl<T: Transport> AsyncRdmaStream<T> {
    /// Wrap a pre-constructed transport as a byte stream.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            recv_pending: None,
            write_pending: None,
            eof: false,
        }
    }

    /// Read data from the stream asynchronously.
    ///
    /// Returns the number of bytes read. Returns `Ok(0)` on disconnect (EOF).
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_read(cx, buf)).await
    }

    /// Write data to the stream asynchronously.
    ///
    /// Returns the number of bytes written (bounded by buffer size).
    pub async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_write(cx, data)).await
    }

    /// Write all data to the stream, looping if necessary.
    pub async fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = self.write(data).await?;
            data = &data[n..];
        }
        Ok(())
    }

    /// Disconnect the stream gracefully.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_close(cx)).await
    }

    /// Get the peer's socket address (remote end of the connection).
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.transport.peer_addr()
    }

    /// Get the local socket address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.transport.local_addr()
    }
}

/// Convenience constructors for the default Send/Recv transport.
impl AsyncRdmaStream<RdmaTransport> {
    /// Connect to a remote RDMA endpoint (client side).
    pub async fn connect(addr: &SocketAddr) -> crate::Result<Self> {
        Self::connect_with_buf_size(addr, DEFAULT_BUF_SIZE).await
    }

    /// Connect with a custom buffer size.
    pub async fn connect_with_buf_size(addr: &SocketAddr, buf_size: usize) -> crate::Result<Self> {
        let config = TransportConfig {
            buf_size,
            ..TransportConfig::stream()
        };
        let transport = RdmaTransport::connect(addr, config).await?;
        Ok(Self::new(transport))
    }
}

impl<T: Transport> Drop for AsyncRdmaStream<T> {
    fn drop(&mut self) {
        let _ = self.transport.disconnect();
    }
}

// --- futures::io trait implementations ---

impl<T: Transport> AsyncRead for AsyncRdmaStream<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if buf.is_empty() || this.eof {
            return Poll::Ready(Ok(0));
        }

        // Phase 1: Return buffered recv data (partial read from previous completion)
        if let Some((buf_idx, offset, total_len)) = this.recv_pending {
            let remaining = total_len - offset;
            let copy_len = remaining.min(buf.len());
            buf[..copy_len]
                .copy_from_slice(&this.transport.recv_buf(buf_idx)[offset..offset + copy_len]);
            if copy_len < remaining {
                this.recv_pending = Some((buf_idx, offset + copy_len, total_len));
            } else {
                this.recv_pending = None;
                this.transport
                    .repost_recv(buf_idx)
                    .map_err(io::Error::other)?;
            }
            return Poll::Ready(Ok(copy_len));
        }

        // Phase 2: Poll transport for new recv completion
        let mut completions = [RecvCompletion::default(); 1];
        match this.transport.poll_recv(cx, &mut completions) {
            Poll::Pending => {
                if this.transport.poll_disconnect(cx) {
                    this.eof = true;
                    return Poll::Ready(Ok(0));
                }
                Poll::Pending
            }
            Poll::Ready(Err(_)) => {
                // FLUSH_ERR etc. — transport marked itself dead
                this.eof = true;
                Poll::Ready(Ok(0))
            }
            Poll::Ready(Ok(n)) if n > 0 => {
                let c = &completions[0];
                if c.byte_len == 0 {
                    return Poll::Ready(Ok(0));
                }
                let copy_len = c.byte_len.min(buf.len());
                buf[..copy_len].copy_from_slice(&this.transport.recv_buf(c.buf_idx)[..copy_len]);
                if copy_len < c.byte_len {
                    this.recv_pending = Some((c.buf_idx, copy_len, c.byte_len));
                } else {
                    this.transport
                        .repost_recv(c.buf_idx)
                        .map_err(io::Error::other)?;
                }
                Poll::Ready(Ok(copy_len))
            }
            _ => Poll::Pending,
        }
    }
}

impl<T: Transport> AsyncWrite for AsyncRdmaStream<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if this.eof || this.transport.is_qp_dead() {
            this.write_pending = None;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection closed",
            )));
        }

        // Post send if not already in progress
        if this.write_pending.is_none() {
            match this.transport.send_copy(buf) {
                Ok(0) => {
                    // All send buffers occupied — fall through to wait
                }
                Ok(n) => {
                    this.write_pending = Some(n);
                }
                Err(e) => return Poll::Ready(Err(io::Error::other(e))),
            }
        }

        // If we haven't posted yet (buffers full), wait then retry
        if this.write_pending.is_none() {
            match this.transport.poll_send_completion(cx) {
                Poll::Pending => {
                    if this.transport.poll_disconnect(cx) {
                        this.eof = true;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "connection closed",
                        )));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    this.eof = true;
                    return Poll::Ready(Err(io::Error::other(e)));
                }
                Poll::Ready(Ok(())) => match this.transport.send_copy(buf) {
                    Ok(n) => this.write_pending = Some(n),
                    Err(e) => return Poll::Ready(Err(io::Error::other(e))),
                },
            }
        }
        let len = this.write_pending.unwrap();

        // Wait for THIS send's completion
        match this.transport.poll_send_completion(cx) {
            Poll::Pending => {
                if this.transport.poll_disconnect(cx) {
                    this.eof = true;
                    this.write_pending = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "connection closed",
                    )));
                }
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                this.eof = true;
                this.write_pending = None;
                Poll::Ready(Err(io::Error::other(e)))
            }
            Poll::Ready(Ok(())) => {
                this.write_pending = None;
                Poll::Ready(Ok(len))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if this.eof || this.transport.is_qp_dead() {
            this.write_pending = None;
            return Poll::Ready(Ok(()));
        }

        // Phase 1: Drain pending send completion before disconnecting.
        if this.write_pending.is_some() {
            match this.transport.poll_send_completion(cx) {
                Poll::Pending => {
                    if this.transport.poll_disconnect(cx) {
                        this.eof = true;
                        this.write_pending = None;
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(_) => {
                    this.write_pending = None;
                }
            }
        }

        // Phase 2: Send DREQ (idempotent).
        if this.eof || this.transport.is_qp_dead() {
            return Poll::Ready(Ok(()));
        }
        let _ = this.transport.disconnect();

        // Phase 3: Await DISCONNECTED event from peer.
        //
        // On rxe (RoCE), the QP may linger in RTS for a significant time
        // after disconnect, and CQ flush completions are unreliable. The CM
        // DISCONNECTED event may have been consumed by a prior poll_disconnect
        // call, leaving nothing to wake the task.
        //
        // We use poll_disconnect in a loop (not a single check) to handle
        // spurious wakeups and to keep retrying the QP state fallback.
        // If poll_disconnect returns Pending, we return Pending — but the
        // CM fd waker is registered, and the QP dead check provides a
        // safety net on the next wake.
        if this.transport.poll_disconnect(cx) {
            this.eof = true;
            Poll::Ready(Ok(()))
        } else {
            // Waker registered on CM fd + QP dead fallback checked.
            Poll::Pending
        }
    }
}

/// An async RDMA listener, analogous to `TcpListener`.
///
/// Binds to a local address and accepts incoming RDMA connections,
/// returning an [`AsyncRdmaStream`] for each.
pub struct AsyncRdmaListener {
    inner: AsyncCmListener,
    buf_size: usize,
}

impl AsyncRdmaListener {
    /// Bind to a local address and start listening.
    pub fn bind(addr: &SocketAddr) -> crate::Result<Self> {
        Self::bind_with_buf_size(addr, DEFAULT_BUF_SIZE)
    }

    /// Bind with a custom buffer size for accepted streams.
    pub fn bind_with_buf_size(addr: &SocketAddr, buf_size: usize) -> crate::Result<Self> {
        let inner = AsyncCmListener::bind(addr)?;
        Ok(Self { inner, buf_size })
    }

    /// Get the local socket address the listener is bound to.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.inner.local_addr()
    }

    /// Accept an incoming connection, returning an [`AsyncRdmaStream`].
    pub async fn accept(&self) -> crate::Result<AsyncRdmaStream<RdmaTransport>> {
        let config = TransportConfig {
            buf_size: self.buf_size,
            ..TransportConfig::stream()
        };
        let transport = RdmaTransport::accept(&self.inner, config).await?;
        Ok(AsyncRdmaStream::new(transport))
    }
}
