//! Transport trait — shared abstraction for RDMA data-path operations.
//!
//! Consumers ([`crate::async_stream::AsyncRdmaStream`]) are generic over
//! `T: Transport`, enabling different transport implementations:
//!
//! - [`crate::rdma_transport::RdmaTransport`] — Send/Recv (two-sided)
//! - (Future) Ring buffer transport — RDMA Write + Immediate Data
//! - Mock transports for unit testing without RDMA hardware
//!
//! The trait is designed around the **consumer's view** (send bytes,
//! receive bytes) rather than RDMA verbs. `WorkCompletion` stays internal
//! to implementations — consumers see only [`RecvCompletion`].

use std::net::SocketAddr;
use std::task::{Context, Poll};

use crate::Result;
use crate::async_cm::AsyncCmListener;

/// Factory for establishing RDMA connections.
///
/// Abstracts the connect/accept pattern so that code can be generic over
/// transport type. Each implementation holds its own configuration
/// (e.g. buffer sizes, ring capacity).
///
/// # Example
///
/// ```ignore
/// async fn connected_pair<B: TransportBuilder>(builder: B) -> (T, T)
/// where B::Transport: Transport
/// {
///     let listener = AsyncCmListener::bind(&addr)?;
///     let server = builder.accept(&listener).await?;
///     let client = builder.connect(&addr).await?;
///     (server, client)
/// }
/// ```
pub trait TransportBuilder: Clone + Send + Sync + Unpin + 'static {
    /// The transport type produced by this builder.
    type Transport: Transport + 'static;

    /// Connect to a remote endpoint (client side).
    fn connect(
        &self,
        addr: &SocketAddr,
    ) -> impl std::future::Future<Output = Result<Self::Transport>> + Send;

    /// Accept an incoming connection (server side).
    fn accept(
        &self,
        listener: &AsyncCmListener,
    ) -> impl std::future::Future<Output = Result<Self::Transport>> + Send;
}

/// A completed receive operation — transport-neutral.
#[derive(Debug, Default, Clone, Copy)]
pub struct RecvCompletion {
    /// Buffer index identifying the received data (valid until [`Transport::repost_recv`]).
    pub buf_idx: usize,
    /// Number of bytes received.
    pub byte_len: usize,
}

/// Shared abstraction for RDMA data-path operations.
///
/// Implementations own the QP, buffers, CQ state, and connection lifecycle.
/// Consumers only interact through this trait — never touching RDMA primitives
/// directly.
///
/// With generics (`T: Transport`), Rust monomorphizes at compile time —
/// zero vtable overhead.
pub trait Transport: Send + Sync {
    // --- Send path ---

    /// Copy data into an internal send buffer and post it.
    ///
    /// Returns bytes accepted (capped by transport capacity).
    /// May return `Ok(0)` if the transport cannot accept data right now
    /// (all send buffers in-flight, or ring full). Caller should call
    /// [`poll_send_completion`](Self::poll_send_completion) and retry.
    ///
    /// For Send/Recv: returns `Ok(0)` when all send buffers are occupied.
    /// For Ring (future): returns `Ok(0)` when remote ring is full.
    fn send_copy(&mut self, data: &[u8]) -> Result<usize>;

    /// Wait for at least one in-flight send to complete.
    ///
    /// Returns `Err` on fatal CQ error (e.g., `WR_FLUSH_ERR` — marks the
    /// transport as dead internally). Returns `Ok(())` when at least one
    /// send completion was successfully reaped.
    fn poll_send_completion(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>>;

    // --- Recv path ---

    /// Poll for recv completions.
    ///
    /// Fills `out` with `RecvCompletion { buf_idx, byte_len }` for each
    /// completed receive. Returns the number of completions written to `out`.
    ///
    /// Returns `Err` on fatal CQ error (e.g., `WR_FLUSH_ERR`).
    ///
    /// Implementations must internally stash any excess completions that
    /// don't fit in `out`. For Ring transport, losing a doorbell's
    /// immediate data means losing the offset/length of data in the ring.
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut [RecvCompletion],
    ) -> Poll<Result<usize>>;

    /// Access received data by buffer index.
    ///
    /// The returned slice is valid until [`repost_recv`](Self::repost_recv)
    /// is called with the same `buf_idx`.
    fn recv_buf(&self, buf_idx: usize) -> &[u8];

    /// Release a recv buffer for reuse.
    ///
    /// Must be called after the consumer has finished reading from the buffer.
    /// For Send/Recv: re-posts the buffer to the receive queue.
    /// For Ring (future): advances the ring head and sends a credit update.
    fn repost_recv(&mut self, buf_idx: usize) -> Result<()>;

    // --- Connection lifecycle ---

    /// Register task waker for disconnect events AND check connection state.
    ///
    /// Returns `true` if the connection is dead (caller should return
    /// EOF or `BrokenPipe`).
    ///
    /// **Must** be called when CQ poll methods return `Pending` — ensures
    /// disconnect events (DREQ → DISCONNECTED) wake the task even when
    /// the CQ fd has no completions to trigger epoll.
    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> bool;

    /// Returns `true` when the connection is dead — no more data will
    /// ever arrive or be sendable.
    fn is_qp_dead(&self) -> bool;

    /// Initiate graceful disconnect. Idempotent — safe to call multiple times.
    fn disconnect(&mut self) -> Result<()>;

    // --- Metadata ---

    /// Local socket address of this connection.
    fn local_addr(&self) -> Option<SocketAddr>;

    /// Remote peer address of this connection.
    fn peer_addr(&self) -> Option<SocketAddr>;
}
