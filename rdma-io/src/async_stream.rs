//! Async RDMA Stream — async `read` + `write` over a [`Transport`].
//!
//! Provides TCP-like async semantics over an RDMA transport, built on
//! the [`Transport`](crate::transport::Transport) trait for completion-driven I/O.
//!
//! # Architecture
//!
//! `AsyncRdmaStream<T>` is generic over `T: Transport`. Callers construct
//! the transport directly (e.g. [`SendRecvTransport::connect`] or
//! [`CreditRingTransport::connect`]) and wrap it with [`AsyncRdmaStream::new`].
//!
//! [`SendRecvTransport::connect`]: crate::send_recv_transport::SendRecvTransport::connect
//! [`CreditRingTransport::connect`]: crate::credit_ring_transport::CreditRingTransport::connect
//!
//! # Protocol
//!
//! No application-level framing. Each `write()` becomes one transport send;
//! each `read()` consumes one recv completion.
//!
//! # Example
//!
//! ```no_run
//! use rdma_io::async_cm::AsyncCmListener;
//! use rdma_io::async_stream::AsyncRdmaStream;
//! use rdma_io::send_recv_transport::{SendRecvTransport, SendRecvConfig};
//!
//! # async fn example() -> rdma_io::Result<()> {
//! // Server
//! let listener = AsyncCmListener::bind(&"0.0.0.0:9999".parse().unwrap())?;
//! let transport = SendRecvTransport::accept(&listener, SendRecvConfig::default()).await?;
//! let mut server = AsyncRdmaStream::new(transport);
//!
//! // Client
//! let addr = "10.0.0.1:9999".parse().unwrap();
//! let transport = SendRecvTransport::connect(&addr, SendRecvConfig::default()).await?;
//! let mut client = AsyncRdmaStream::new(transport);
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};

use crate::transport::{RecvCompletion, Transport};

/// Upper bound on bytes coalesced into a single send by
/// [`poll_write_vectored`](AsyncRdmaStream::poll_write_vectored).
///
/// Vectored writes gather the caller's slices (e.g. an HTTP/2 frame header
/// plus its payload) into one contiguous buffer so they travel as a single
/// RDMA message instead of one send per slice. The gather is capped so the
/// reusable scratch buffer stays bounded; anything beyond the cap (or beyond a
/// transport's per-send capacity) is left for the next write. 64 KiB matches
/// the Send/Recv stream buffer and exceeds the ring transports' per-message
/// limit, so it never under-fills a single send.
const MAX_GATHER: usize = 64 * 1024;

/// An async RDMA stream with `read` and `write` methods.
///
/// Generic over `T: Transport`. Construct via [`AsyncRdmaStream::new`] with
/// a pre-built transport.
pub struct AsyncRdmaStream<T: Transport> {
    transport: T,
    /// Partially consumed recv: (buf_index, offset, total_len).
    recv_pending: Option<(usize, usize, usize)>,
    /// In-flight send length. None if send slot is free.
    write_pending: Option<usize>,
    /// Reusable buffer for coalescing vectored writes into one send.
    /// Grows to a high-water mark (capped at [`MAX_GATHER`]); reused across
    /// calls to avoid per-write allocation.
    write_scratch: Vec<u8>,
    /// Set when transport returns Err from poll_recv (QP entered ERROR state).
    /// Once set, poll_read always returns Ok(0) — the QP will never
    /// produce another recv completion.
    eof: bool,
}

// T: Transport is Send + Sync, and our own fields are trivially Unpin/Send/Sync.
impl<T: Transport> Unpin for AsyncRdmaStream<T> {}

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
            write_scratch: Vec::new(),
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

impl<T: Transport> AsyncRdmaStream<T> {
    /// Core send state machine for a single contiguous slice.
    ///
    /// Posts `buf` as one transport send (if none is in flight) and drives it
    /// to completion. Only one send is outstanding at a time (`write_pending`),
    /// so a re-poll after `Pending` skips re-posting and just waits for the
    /// in-flight send. Shared by [`poll_write`](AsyncWrite::poll_write) and the
    /// coalescing [`poll_write_vectored`](AsyncWrite::poll_write_vectored).
    fn poll_write_slice(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self;

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if this.eof {
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
                    // All send buffers occupied or credits exhausted.
                    // Poll recv CQ to register a waker for incoming credit
                    // updates (ring transport sends credits via Send+Imm on
                    // the recv CQ). Without this, credit-blocked writes deadlock
                    // because poll_send_completion only watches the send CQ.
                    let mut completions = [RecvCompletion::default(); 1];
                    let _ = this.transport.poll_recv(cx, &mut completions);
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
                    Ok(0) => {
                        // Still blocked (credit exhaustion for ring transport).
                        // Poll recv CQ to register a waker — credit updates
                        // arrive as Send+Imm on the recv CQ, not the send CQ.
                        let mut completions = [RecvCompletion::default(); 1];
                        let _ = this.transport.poll_recv(cx, &mut completions);
                        return Poll::Pending;
                    }
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

    /// Coalesce vectored slices into a single send.
    ///
    /// Shared by the `futures_io` and `tokio` [`AsyncWrite`] impls; see
    /// [`AsyncWrite::poll_write_vectored`] for the rationale.
    fn poll_write_vectored_impl(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        // Locate non-empty slices to pick the cheapest path.
        let mut first_nonempty = None;
        let mut nonempty_count = 0usize;
        for (i, b) in bufs.iter().enumerate() {
            if !b.is_empty() {
                nonempty_count += 1;
                if first_nonempty.is_none() {
                    first_nonempty = Some(i);
                }
            }
        }

        match (nonempty_count, first_nonempty) {
            // Nothing to write.
            (0, _) => return Poll::Ready(Ok(0)),
            // Exactly one slice carries data — send it directly, no gather copy.
            (1, Some(i)) => return self.poll_write_slice(cx, &bufs[i]),
            _ => {}
        }

        // Coalesce the slices into the reusable scratch buffer. Only rebuild
        // when no send is in flight; while a send is pending the scratch must
        // keep the exact bytes/length already posted so the state machine can
        // complete it (a re-poll after `Pending` presents the same slices).
        if self.write_pending.is_none() {
            self.write_scratch.clear();
            for b in bufs {
                if b.is_empty() {
                    continue;
                }
                let remaining = MAX_GATHER - self.write_scratch.len();
                if remaining == 0 {
                    break;
                }
                let take = b.len().min(remaining);
                self.write_scratch.extend_from_slice(&b[..take]);
                if take < b.len() {
                    break;
                }
            }
        }

        // Swap the scratch out so `poll_write_slice` can take `&mut self`.
        let scratch = std::mem::take(&mut self.write_scratch);
        let res = self.poll_write_slice(cx, &scratch);
        self.write_scratch = scratch;
        res
    }

    /// Graceful close: drain any in-flight send, then send DREQ.
    ///
    /// Shared by the `futures_io` `poll_close` and the `tokio` `poll_shutdown`.
    fn poll_close_impl(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.eof {
            self.write_pending = None;
            return Poll::Ready(Ok(()));
        }

        // Phase 1: Drain pending send completion before disconnecting.
        if self.write_pending.is_some() {
            match self.transport.poll_send_completion(cx) {
                Poll::Pending => {
                    if self.transport.poll_disconnect(cx) {
                        self.eof = true;
                        self.write_pending = None;
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(_) => {
                    self.write_pending = None;
                }
            }
        }

        // Phase 2: Send DREQ and complete.
        //
        // We don't wait for the peer's DREP or DISCONNECTED event.
        // On rxe, the peer may not process DREQ promptly (e.g. idle
        // server), causing an 80+ second CM timeout. The kernel handles
        // the DREP exchange asynchronously, and Drop performs final
        // QP/CQ cleanup.
        let _ = self.transport.disconnect();
        self.eof = true;
        Poll::Ready(Ok(()))
    }

    /// Native `tokio` read into a [`ReadBuf`], copying only the received bytes.
    ///
    /// Unlike the `futures_io` → `tokio` `Compat` bridge (which calls
    /// `ReadBuf::initialize_unfilled`, zero-filling the whole spare capacity on
    /// every poll), this writes recv data straight into the unfilled region via
    /// `put_slice` — no per-read memset of the (up to 64 KiB) stream buffer.
    #[cfg(feature = "tokio")]
    fn poll_read_tokio(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 || self.eof {
            return Poll::Ready(Ok(()));
        }

        // Phase 1: Return buffered recv data (partial read from previous completion)
        if let Some((buf_idx, offset, total_len)) = self.recv_pending {
            let remaining = total_len - offset;
            let copy_len = remaining.min(buf.remaining());
            buf.put_slice(&self.transport.recv_buf(buf_idx)[offset..offset + copy_len]);
            if copy_len < remaining {
                self.recv_pending = Some((buf_idx, offset + copy_len, total_len));
            } else {
                self.recv_pending = None;
                let _ = self.transport.repost_recv(buf_idx);
            }
            return Poll::Ready(Ok(()));
        }

        // Phase 2: Poll transport for new recv completion. Loop drains
        // credit-only batches (Ok(0)) so the CQ waker is re-registered.
        let mut completions = [RecvCompletion::default(); 1];
        loop {
            match self.transport.poll_recv(cx, &mut completions) {
                Poll::Pending => {
                    if self.transport.poll_disconnect(cx) {
                        self.eof = true;
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(_)) => {
                    self.eof = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Ok(0)) => continue,
                Poll::Ready(Ok(_)) => {
                    let c = completions[0];
                    if c.byte_len == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    let copy_len = c.byte_len.min(buf.remaining());
                    buf.put_slice(&self.transport.recv_buf(c.buf_idx)[..copy_len]);
                    if copy_len < c.byte_len {
                        self.recv_pending = Some((c.buf_idx, copy_len, c.byte_len));
                    } else {
                        let _ = self.transport.repost_recv(c.buf_idx);
                    }
                    return Poll::Ready(Ok(()));
                }
            }
        }
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
                // Repost failure is non-fatal — data is already delivered.
                // The buffer is lost, but the QP ERROR will be detected on next poll.
                let _ = this.transport.repost_recv(buf_idx);
            }
            return Poll::Ready(Ok(copy_len));
        }

        // Phase 2: Poll transport for new recv completion.
        // Loop handles credit-only batches (Ok(0)) from ring transport —
        // credits update internal state but produce no data. Re-polling
        // ensures the CQ waker is properly registered when no data remains.
        let mut completions = [RecvCompletion::default(); 1];
        loop {
            match this.transport.poll_recv(cx, &mut completions) {
                Poll::Pending => {
                    if this.transport.poll_disconnect(cx) {
                        this.eof = true;
                        return Poll::Ready(Ok(0));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(_)) => {
                    // FLUSH_ERR etc. — transport marked itself dead
                    this.eof = true;
                    return Poll::Ready(Ok(0));
                }
                Poll::Ready(Ok(0)) => {
                    // Credit-only or internal-state-only batch — re-poll.
                    continue;
                }
                Poll::Ready(Ok(_)) => {
                    let c = &completions[0];
                    if c.byte_len == 0 {
                        return Poll::Ready(Ok(0));
                    }
                    let copy_len = c.byte_len.min(buf.len());
                    buf[..copy_len]
                        .copy_from_slice(&this.transport.recv_buf(c.buf_idx)[..copy_len]);
                    if copy_len < c.byte_len {
                        this.recv_pending = Some((c.buf_idx, copy_len, c.byte_len));
                    } else {
                        let _ = this.transport.repost_recv(c.buf_idx);
                    }
                    return Poll::Ready(Ok(copy_len));
                }
            }
        }
    }
}

impl<T: Transport> AsyncWrite for AsyncRdmaStream<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_slice(cx, buf)
    }

    /// Gather multiple slices into a single RDMA send.
    ///
    /// Higher layers (hyper/h2) hand us an HTTP/2 frame as several
    /// discontiguous slices — a 9-byte frame header, HPACK bytes, then the
    /// payload. Without this, each slice would become its own `send_copy`
    /// (one RDMA message + a signalled completion wait apiece). Coalescing
    /// them into one contiguous buffer collapses a request into a single
    /// message with one completion.
    ///
    /// Only the first [`MAX_GATHER`] bytes are coalesced; the returned count
    /// (and `send_copy`'s own per-send cap) leaves any remainder for the next
    /// call, which the caller re-presents.
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_vectored_impl(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_close_impl(cx)
    }
}

// --- tokio::io trait implementations ---
//
// Native `tokio` traits so consumers (e.g. `rdma-io-tonic`) can use the stream
// directly, without the `futures_io` → `tokio` `Compat` shim. This avoids the
// shim's defensive `ReadBuf::initialize_unfilled` zero-fill on every read and
// its lack of vectored-write forwarding.

#[cfg(feature = "tokio")]
impl<T: Transport> tokio::io::AsyncRead for AsyncRdmaStream<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut().poll_read_tokio(cx, buf)
    }
}

#[cfg(feature = "tokio")]
impl<T: Transport> tokio::io::AsyncWrite for AsyncRdmaStream<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_slice(cx, buf)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_vectored_impl(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_close_impl(cx)
    }
}
