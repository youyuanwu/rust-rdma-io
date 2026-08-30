//! Message-oriented Send/Recv transport over RDMA.
//!
//! Provides [`MessageTransport`] with pre-registered reusable buffer pools,
//! message boundaries, bounded backpressure, and cancellation-safe
//! `send()`/`recv()` operations.
//!
//! # Receive-Buffer Invariant
//!
//! Every configured receive MR is in exactly one of these states at any time:
//! - **Posted** — registered in the inflight map and posted to the QP as a recv WR
//! - **Delivered** — completed by the HCA, sitting in the recv channel or held
//!   by a [`ReceivedMessage`] handle
//! - **Queued for repost** — returned by [`ReceivedMessage::drop`] to the repost
//!   channel, awaiting re-posting by the recv pump
//! - **Teardown-owned** — transport is shutting down; the MR will be dropped
//!
//! No MR is ever lost, double-posted, or accessed concurrently by the
//! application and the HCA.
//!
//! # Usage
//!
//! ```no_run
//! # use rdma_io::v2::*;
//! # use rdma_io::v2::message_transport::*;
//! # async fn example() -> Result<()> {
//! let transport = MessageTransportBuilder::new()
//!     .recv_buffers(32)
//!     .send_buffers(16)
//!     .buffer_size(64 * 1024)
//!     .completion_mode(CompletionMode::Readiness)
//!     .connect("192.168.1.1:7471".parse().unwrap())
//!     .await?;
//!
//! transport.send(b"hello").await?;
//! let msg = transport.recv().await?;
//! assert_eq!(msg.as_ref(), b"hello");
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::async_cm::AsyncCmListener;
use crate::cm::ConnParam;

use super::connection::{CompletionMode, Connection, ConnectionBuilder, ConnectionConfig};
use super::driver::CqDriverHandle;
use super::error::{Error, Result};
use super::mr::{AccessIntent, Mr};
use super::pd::Pd;
use super::shared_qp::OpFuture;

/// Builder for creating a [`MessageTransport`].
///
/// This is the complete public configuration surface for the v2 message
/// transport. All RDMA resource configuration is exposed here; internal
/// builders handle the resource wiring.
///
/// # Defaults
///
/// | Parameter | Default |
/// |-----------|---------|
/// | recv_buffers | 32 |
/// | send_buffers | 16 |
/// | buffer_size | 65536 (64 KB) |
/// | completion_mode | Readiness |
/// | separate_cqs | false |
///
/// # Errors
///
/// Builder methods do not fail. Validation occurs at `connect()` or
/// `accept()` time; invalid configuration produces [`Error::InvalidConfig`].
pub struct MessageTransportBuilder {
    recv_buffer_count: usize,
    send_buffer_count: usize,
    buffer_size: usize,
    completion_mode: CompletionMode,
    conn_param: ConnParam,
    inflight_capacity: Option<usize>,
    separate_cqs: bool,
}

impl Default for MessageTransportBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageTransportBuilder {
    /// Create a new builder with default configuration.
    pub fn new() -> Self {
        Self {
            recv_buffer_count: 32,
            send_buffer_count: 16,
            buffer_size: 64 * 1024,
            completion_mode: CompletionMode::Readiness,
            conn_param: ConnParam::default(),
            inflight_capacity: None,
            separate_cqs: false,
        }
    }

    /// Set the number of pre-posted receive buffers.
    pub fn recv_buffers(mut self, count: usize) -> Self {
        self.recv_buffer_count = count;
        self
    }

    /// Set the number of reusable send buffers.
    pub fn send_buffers(mut self, count: usize) -> Self {
        self.send_buffer_count = count;
        self
    }

    /// Set the maximum message size in bytes.
    pub fn buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// Set the CQ completion integration mode.
    pub fn completion_mode(mut self, mode: CompletionMode) -> Self {
        self.completion_mode = mode;
        self
    }

    /// Set RDMA connection parameters.
    pub fn conn_param(mut self, param: ConnParam) -> Self {
        self.conn_param = param;
        self
    }

    /// Override the in-flight registry capacity.
    ///
    /// Default: derived from `send_buffers + recv_buffers + 2` (the extra
    /// slots accommodate the WR headroom on each direction).
    pub fn inflight_capacity(mut self, capacity: usize) -> Self {
        self.inflight_capacity = Some(capacity);
        self
    }

    /// Use separate CQs for send and receive directions.
    ///
    /// Default: `false` (one shared CQ for both directions).
    pub fn separate_cqs(mut self, separate: bool) -> Self {
        self.separate_cqs = separate;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.recv_buffer_count == 0 {
            return Err(Error::InvalidConfig("recv_buffers must be > 0".into()));
        }
        if self.send_buffer_count == 0 {
            return Err(Error::InvalidConfig("send_buffers must be > 0".into()));
        }
        if self.buffer_size == 0 {
            return Err(Error::InvalidConfig("buffer_size must be > 0".into()));
        }
        Ok(())
    }

    fn derive_config(&self) -> ConnectionConfig {
        let max_send_wr = self.send_buffer_count + 1;
        let max_recv_wr = self.recv_buffer_count + 1;
        let total_wr = max_send_wr + max_recv_wr;
        let inflight = self.inflight_capacity.unwrap_or(total_wr);
        let cq_depth = total_wr + 4; // headroom

        ConnectionConfig {
            completion_mode: self.completion_mode,
            cq_depth,
            max_send_wr,
            max_recv_wr,
            inflight_capacity: inflight,
            conn_param: self.conn_param.clone(),
            separate_cqs: self.separate_cqs,
        }
    }

    /// Connect to a remote endpoint and create a transport (client side).
    ///
    /// Allocates all MRs, posts receive buffers (before the CM handshake),
    /// then completes the RDMA connection and returns a ready-to-use transport.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] on invalid builder parameters, or
    /// any verbs/CM error during resource allocation and connection setup.
    pub async fn connect(self, addr: SocketAddr) -> Result<MessageTransport> {
        self.validate()?;
        let send_count = self.send_buffer_count;
        let recv_count = self.recv_buffer_count;
        let buf_size = self.buffer_size;
        let config = self.derive_config();

        // Pre-allocated recv state: recv MRs are allocated and posted
        // inside the pre_establish callback (before CM handshake) so that
        // the QP has posted recv WRs before either side can send traffic.
        let pre_posted: std::sync::Mutex<Option<Vec<OpFuture>>> = std::sync::Mutex::new(None);

        let builder = ConnectionBuilder::new(config)?;
        let conn = builder
            .connect(&addr, |sqp, pd| {
                let futures = allocate_and_prepost_recvs(
                    sqp.qp(),
                    sqp.recv_handle(),
                    pd,
                    recv_count,
                    buf_size,
                )?;
                *pre_posted.lock().unwrap() = Some(futures);
                Ok(())
            })
            .await?;

        let pre_posted = pre_posted
            .into_inner()
            .unwrap()
            .ok_or_else(|| Error::InvalidConfig("pre_establish not called".into()))?;

        MessageTransport::from_connection(conn, send_count, buf_size, pre_posted).await
    }

    /// Accept a connection and create a transport (server side).
    ///
    /// Allocates all MRs, posts receive buffers (before the CM `accept()`),
    /// then completes the RDMA handshake and returns a ready-to-use transport.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] on invalid builder parameters, or
    /// any verbs/CM error during resource allocation and connection setup.
    pub async fn accept(self, listener: &AsyncCmListener) -> Result<MessageTransport> {
        self.validate()?;
        let send_count = self.send_buffer_count;
        let recv_count = self.recv_buffer_count;
        let buf_size = self.buffer_size;
        let config = self.derive_config();

        let pre_posted: std::sync::Mutex<Option<Vec<OpFuture>>> = std::sync::Mutex::new(None);

        let builder = ConnectionBuilder::new(config)?;
        let conn = builder
            .accept(listener, |sqp, pd| {
                let futures = allocate_and_prepost_recvs(
                    sqp.qp(),
                    sqp.recv_handle(),
                    pd,
                    recv_count,
                    buf_size,
                )?;
                *pre_posted.lock().unwrap() = Some(futures);
                Ok(())
            })
            .await?;

        let pre_posted = pre_posted
            .into_inner()
            .unwrap()
            .ok_or_else(|| Error::InvalidConfig("pre_establish not called".into()))?;

        MessageTransport::from_connection(conn, send_count, buf_size, pre_posted).await
    }
}

/// Allocate receive MRs and post them as recv WRs before the CM handshake.
///
/// Returns a `Vec<OpFuture>` for each posted recv, already in the inflight
/// state. These are handed to the recv pump task which owns them.
fn allocate_and_prepost_recvs(
    qp: &Arc<super::qp::Qp>,
    handle: &Arc<CqDriverHandle>,
    pd: &Pd,
    count: usize,
    buffer_size: usize,
) -> Result<Vec<OpFuture>> {
    let mut futures = Vec::with_capacity(count);
    for _ in 0..count {
        let mr = pd.reg_mr(buffer_size, AccessIntent::LocalOnly)?;
        let future = post_recv_and_track(qp, handle, mr)?;
        futures.push(future);
    }
    Ok(futures)
}

/// A received message with its exact byte length.
///
/// Wraps a registered MR and exposes only the received bytes. When
/// dropped, the backing MR is returned to the transport's receive
/// pool for reposting.
///
/// # Cancellation Safety
///
/// Dropping a `ReceivedMessage` safely returns its buffer for reposting.
/// The repost channel is unbounded so `Drop` never blocks.
pub struct ReceivedMessage {
    mr: Option<Mr>,
    byte_len: usize,
    repost_tx: mpsc::UnboundedSender<Mr>,
}

impl ReceivedMessage {
    /// The received byte length.
    pub fn len(&self) -> usize {
        self.byte_len
    }

    /// Whether the received message is empty (zero-length).
    pub fn is_empty(&self) -> bool {
        self.byte_len == 0
    }
}

impl AsRef<[u8]> for ReceivedMessage {
    fn as_ref(&self) -> &[u8] {
        let mr = self.mr.as_ref().expect("ReceivedMessage used after take");
        &mr.as_slice()[..self.byte_len]
    }
}

impl std::ops::Deref for ReceivedMessage {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl Drop for ReceivedMessage {
    fn drop(&mut self) {
        if let Some(mr) = self.mr.take() {
            // Return MR to recv pump for reposting. The unbounded channel
            // cannot fail unless the receiver is dropped (transport shutdown),
            // in which case the MR is simply dropped.
            let _ = self.repost_tx.send(mr);
        }
    }
}

impl std::fmt::Debug for ReceivedMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceivedMessage")
            .field("byte_len", &self.byte_len)
            .finish()
    }
}

/// Internal struct for a completed receive to pass through the channel.
struct CompletedRecv {
    mr: Mr,
    byte_len: usize,
}

/// Message-oriented RDMA transport.
///
/// Provides async `send()` and `recv()` with pre-registered reusable
/// buffer pools, message boundaries, and bounded backpressure.
///
/// # Send Semantics
///
/// `send().await` means the local send completion (CQE) was received
/// with success status. It does **not** mean the remote peer has consumed
/// the message. The message may still be in the remote NIC's receive
/// buffer or the peer's recv channel.
///
/// # Backpressure
///
/// Concurrent sends beyond the send buffer pool size wait asynchronously
/// for a buffer to become available. The sender cannot overrun remote
/// receive capacity because send buffers are bounded locally. RC RNR
/// retry (`rnr_retry_count = 7`, effectively infinite) acts as a transport
/// layer safety net — it is NOT the primary flow-control mechanism.
///
/// # Ordering
///
/// Message ordering is guaranteed by RC QP semantics for WRs posted
/// by a single task. Concurrent multi-task sends may interleave at QP
/// level.
///
/// # Cancellation Safety
///
/// - Cancelling `send()` after posting: the MR returns to the send pool
///   when the CQE arrives via the `on_cancel_reclaim` callback.
/// - Cancelling `recv()`: the message stays in the internal channel
///   for the next `recv()` call. No message is lost.
pub struct MessageTransport {
    connection: Connection,
    buffer_size: usize,
    /// Send pool: bounded channel of available send MRs.
    send_pool_tx: mpsc::Sender<Mr>,
    send_pool_rx: Arc<Mutex<mpsc::Receiver<Mr>>>,
    /// Receive message channel: completed receives waiting for recv().
    recv_msg_rx: Arc<Mutex<mpsc::Receiver<CompletedRecv>>>,
    /// Repost channel: MRs returned from ReceivedMessage for reposting.
    repost_tx: mpsc::UnboundedSender<Mr>,
    /// Recv pump task handle.
    recv_pump_task: Option<JoinHandle<()>>,
    /// Disconnect monitor task handle.
    disconnect_task: Option<JoinHandle<()>>,
    /// Shutdown signal — when closed, all pump/monitor tasks exit.
    shutdown_tx: mpsc::Sender<()>,
}

impl MessageTransport {
    /// Build a transport from an established connection, pre-posted recv
    /// futures, and send buffer configuration.
    async fn from_connection(
        connection: Connection,
        send_count: usize,
        buffer_size: usize,
        pre_posted_recvs: Vec<OpFuture>,
    ) -> Result<Self> {
        let pd = connection.pd().clone();

        // Allocate send buffers
        let (send_pool_tx, send_pool_rx) = mpsc::channel(send_count);
        for _ in 0..send_count {
            let mr = pd.reg_mr(buffer_size, AccessIntent::LocalOnly)?;
            send_pool_tx
                .send(mr)
                .await
                .map_err(|_| Error::InvalidConfig("send pool channel closed".into()))?;
        }

        // Receive channels
        let recv_count = pre_posted_recvs.len();
        let (recv_msg_tx, recv_msg_rx) = mpsc::channel(recv_count);
        let (repost_tx, repost_rx) = mpsc::unbounded_channel();

        // Shutdown signal (capacity 1 — one signal is enough)
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let pump_qp = connection.shared_qp().qp().clone();
        let pump_handle = if connection.driver_handles().len() > 1 {
            // Separate-CQ mode: recv handle is the second
            connection.driver_handles()[1].clone()
        } else {
            // Shared-CQ mode: same handle for both
            connection.driver_handles()[0].clone()
        };

        // Clone senders for disconnect monitor to close
        let disc_recv_tx = recv_msg_tx.clone();
        let disc_send_pool_tx = send_pool_tx.clone();
        let disc_shutdown_rx = {
            let (tx, rx) = mpsc::channel::<()>(1);
            // The disconnect monitor will be notified via its own
            // shutdown channel derived from the main one
            drop(tx);
            rx
        };

        let recv_pump_task = tokio::spawn(recv_pump(
            pump_qp,
            pump_handle,
            pd,
            pre_posted_recvs,
            recv_msg_tx,
            repost_rx,
            shutdown_rx,
        ));

        // Disconnect monitor is not spawned by default because the CM
        // event channel fd is consumed by Connection's AsyncFd. The
        // connection shutdown path (Connection::initiate_shutdown) handles
        // QP→error + flush_and_shutdown, which resolves all pending ops.
        // If the peer disconnects, the recv pump sees flush errors and exits.
        let _ = (disc_recv_tx, disc_send_pool_tx, disc_shutdown_rx);

        Ok(Self {
            connection,
            buffer_size,
            send_pool_tx,
            send_pool_rx: Arc::new(Mutex::new(send_pool_rx)),
            recv_msg_rx: Arc::new(Mutex::new(recv_msg_rx)),
            repost_tx,
            recv_pump_task: Some(recv_pump_task),
            disconnect_task: None,
            shutdown_tx,
        })
    }

    /// Send a message. Returns when the local send completion arrives.
    ///
    /// `send().await` is local-completion only — it does NOT guarantee
    /// remote consumption. The message may still be in the remote NIC's
    /// receive buffer.
    ///
    /// # Errors
    ///
    /// - [`Error::MessageTooLarge`] if `data.len() > buffer_size`
    /// - [`Error::TransportClosed`] if the transport is shut down or
    ///   disconnected
    /// - [`Error::CompletionError`] if the send WR completed with error
    ///
    /// # Cancellation Safety
    ///
    /// If cancelled after a send MR is acquired but before the CQE
    /// arrives, the `on_cancel_reclaim` callback returns the MR to the
    /// send pool when the HCA finishes the operation.
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        if data.len() > self.buffer_size {
            return Err(Error::MessageTooLarge {
                size: data.len(),
                capacity: self.buffer_size,
            });
        }

        // Acquire a send buffer from the pool (backpressure point)
        let mut mr = {
            let mut rx = self.send_pool_rx.lock().await;
            rx.recv().await.ok_or(Error::TransportClosed)?
        };

        // Copy data into the registered MR
        mr.as_mut_slice()[..data.len()].copy_from_slice(data);

        // Create the send OpFuture with cancel reclaim callback
        let send_pool_tx = self.send_pool_tx.clone();
        let op = self
            .connection
            .shared_qp()
            .send(mr, Some((0, data.len())))
            .on_cancel_reclaim(Box::new(move |mr| {
                // Return the MR to the send pool on cancellation.
                // try_send may fail if pool is closed (shutdown) — MR drops.
                let _ = send_pool_tx.try_send(mr);
            }));

        // Await local send completion
        let (result, mr) = op.await;

        // Map flush errors to TransportClosed at the transport boundary
        let result = result.map_err(|e| match &e {
            Error::CompletionError { status, .. } if *status == crate::wc::WcStatus::WrFlushErr => {
                Error::TransportClosed
            }
            _ => e,
        });
        result?;

        // Return MR to the send pool
        let _ = self.send_pool_tx.try_send(mr);

        Ok(())
    }

    /// Receive a message. Returns the next completed message.
    ///
    /// Multiple concurrent `recv()` calls are supported. Each message
    /// is delivered to exactly one receiver (FIFO from the internal
    /// mpsc channel serialized by a Mutex).
    ///
    /// # Cancellation Safety
    ///
    /// Cancelling `recv()` does not consume or lose any message.
    /// The message remains in the internal channel for the next caller.
    ///
    /// # Errors
    ///
    /// - [`Error::TransportClosed`] if the transport is shut down or
    ///   disconnected
    pub async fn recv(&self) -> Result<ReceivedMessage> {
        let completed = {
            let mut rx = self.recv_msg_rx.lock().await;
            rx.recv().await.ok_or(Error::TransportClosed)?
        };

        Ok(ReceivedMessage {
            mr: Some(completed.mr),
            byte_len: completed.byte_len,
            repost_tx: self.repost_tx.clone(),
        })
    }

    /// The configured maximum message size.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Access the underlying connection.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Graceful async shutdown.
    ///
    /// Closes all internal channels, aborts the recv pump, and performs
    /// a bounded-timeout await of the connection driver tasks.
    pub async fn close(mut self) {
        // Signal shutdown
        let _ = self.shutdown_tx.send(()).await;

        // Abort recv pump
        if let Some(task) = self.recv_pump_task.take() {
            task.abort();
            let _ = task.await;
        }

        // Abort disconnect monitor
        if let Some(task) = self.disconnect_task.take() {
            task.abort();
            let _ = task.await;
        }

        // Graceful connection close — since we can't move out of self
        // (which implements Drop), we initiate shutdown synchronously.
        self.connection.initiate_shutdown();
    }
}

impl Drop for MessageTransport {
    fn drop(&mut self) {
        // Abort recv pump (synchronous)
        if let Some(task) = self.recv_pump_task.take() {
            task.abort();
        }
        // Abort disconnect monitor
        if let Some(task) = self.disconnect_task.take() {
            task.abort();
        }
        // Connection::drop handles QP→error + driver shutdown
    }
}

/// Background task that manages receive buffer posting and completion routing.
///
/// Starts with already-posted recv OpFutures (from pre_establish). When a
/// recv completes, the MR and byte length are sent to the recv channel.
/// When a ReceivedMessage is dropped, the MR comes back via the repost
/// channel and is re-posted as a new recv WR.
///
/// # Buffer Invariant
///
/// Every recv MR is exactly one of:
/// - In `pending_recvs` (posted to QP, awaiting HCA completion)
/// - In `recv_msg_tx` channel (completed, waiting for `recv()`)
/// - Held by a `ReceivedMessage` (application-owned)
/// - In `repost_rx` channel (returned, waiting to be re-posted)
async fn recv_pump(
    qp: Arc<super::qp::Qp>,
    handle: Arc<CqDriverHandle>,
    _pd: Pd, // kept alive so MRs remain valid
    initial_futures: Vec<OpFuture>,
    recv_msg_tx: mpsc::Sender<CompletedRecv>,
    mut repost_rx: mpsc::UnboundedReceiver<Mr>,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    let mut pending_recvs: Vec<OpFuture> = initial_futures;

    loop {
        if pending_recvs.is_empty() {
            // All buffers are out — wait for repost or shutdown
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => return,
                mr = repost_rx.recv() => {
                    match mr {
                        Some(mr) => {
                            match post_recv_and_track(&qp, &handle, mr) {
                                Ok(future) => pending_recvs.push(future),
                                Err(_) => return, // QP in error
                            }
                        }
                        None => return, // Transport dropped
                    }
                    continue;
                }
            }
        }

        // Wait for: a recv completion, a reposted MR, or shutdown
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => return,
            // Poll pending recvs for the first completion
            result = poll_any_ready(&mut pending_recvs) => {
                match result {
                    PollResult::Ready(idx, Ok((completion, mr))) => {
                        pending_recvs.swap_remove(idx);
                        let byte_len = completion.byte_len() as usize;
                        if recv_msg_tx.send(CompletedRecv { mr, byte_len }).await.is_err() {
                            return; // recv channel closed
                        }
                    }
                    PollResult::Ready(idx, Err((_err, _mr))) => {
                        pending_recvs.swap_remove(idx);
                        // Flush/error completion — transport shutting down.
                        // Don't return immediately: drain remaining completions
                        // so MRs are not leaked to detached reclaim.
                        tracing::debug!("recv_pump: recv completion error, draining");
                        // Drop remaining futures (they'll push to reclaim queue)
                        pending_recvs.clear();
                        return;
                    }
                    PollResult::AllPending => {
                        // Also check for reposts while waiting
                        tokio::select! {
                            biased;
                            _ = shutdown_rx.recv() => return,
                            mr = repost_rx.recv() => {
                                match mr {
                                    Some(mr) => {
                                        match post_recv_and_track(&qp, &handle, mr) {
                                            Ok(future) => pending_recvs.push(future),
                                            Err(_) => return,
                                        }
                                    }
                                    None => return,
                                }
                            }
                            result = poll_any_ready(&mut pending_recvs) => {
                                match result {
                                    PollResult::Ready(idx, Ok((completion, mr))) => {
                                        pending_recvs.swap_remove(idx);
                                        let byte_len = completion.byte_len() as usize;
                                        if recv_msg_tx.send(CompletedRecv { mr, byte_len }).await.is_err() {
                                            return;
                                        }
                                    }
                                    PollResult::Ready(idx, Err(_)) => {
                                        pending_recvs.swap_remove(idx);
                                        pending_recvs.clear();
                                        return;
                                    }
                                    PollResult::AllPending => {
                                        // Yield to avoid busy-spinning
                                        tokio::task::yield_now().await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Drain repost channel eagerly
            mr = repost_rx.recv() => {
                match mr {
                    Some(mr) => {
                        match post_recv_and_track(&qp, &handle, mr) {
                            Ok(future) => pending_recvs.push(future),
                            Err(_) => return,
                        }
                    }
                    None => return,
                }
            }
        }
    }
}

/// Result of polling pending recv futures.
enum PollResult {
    /// Future at index completed with the given result.
    Ready(
        usize,
        std::result::Result<(super::op::Completion, Mr), (Error, Mr)>,
    ),
    /// All futures are still pending.
    AllPending,
}

/// Poll all pending recv futures and return the first one that's ready.
///
/// # Safety
///
/// OpFuture is `Unpin` (it does not use pinning projections — its state
/// machine moves data between enum variants via `std::mem::replace`).
async fn poll_any_ready(pending: &mut [OpFuture]) -> PollResult {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Poll;

    std::future::poll_fn(|cx| {
        for (i, fut) in pending.iter_mut().enumerate() {
            // OpFuture is Unpin — Pin::new is safe
            let pinned = Pin::new(fut);
            if let Poll::Ready((result, mr)) = pinned.poll(cx) {
                let mapped = match result {
                    Ok(completion) => Ok((completion, mr)),
                    Err(e) => Err((e, mr)),
                };
                return Poll::Ready(PollResult::Ready(i, mapped));
            }
        }
        Poll::Ready(PollResult::AllPending)
    })
    .await
}

/// Post a receive WR directly (bypassing SharedQp) and return an
/// already-inflight OpFuture.
///
/// Used by both the pre_establish callback and the recv pump for reposts.
fn post_recv_and_track(
    qp: &Arc<super::qp::Qp>,
    handle: &Arc<CqDriverHandle>,
    mr: Mr,
) -> Result<OpFuture> {
    let len = mr.len();

    let reg = handle.map().register().ok_or(Error::CapacityExhausted)?;
    let token = reg.token;

    let addr = mr.addr();
    let sge = crate::wr::Sge::new(addr, len as u32, mr.lkey());
    let mut wr = crate::wr::RecvWr::new(token).sg(sge);

    if let Err(e) = qp.post_recv_wr_raw(&mut wr) {
        handle.map().release(token);
        return Err(e);
    }

    Ok(OpFuture::new_inflight(handle.clone(), token, mr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let builder = MessageTransportBuilder::new();
        assert_eq!(builder.recv_buffer_count, 32);
        assert_eq!(builder.send_buffer_count, 16);
        assert_eq!(builder.buffer_size, 64 * 1024);
        assert!(!builder.separate_cqs);
    }

    #[test]
    fn test_builder_validation_zero_recv() {
        let builder = MessageTransportBuilder::new().recv_buffers(0);
        assert!(builder.validate().is_err());
    }

    #[test]
    fn test_builder_validation_zero_send() {
        let builder = MessageTransportBuilder::new().send_buffers(0);
        assert!(builder.validate().is_err());
    }

    #[test]
    fn test_builder_validation_zero_size() {
        let builder = MessageTransportBuilder::new().buffer_size(0);
        assert!(builder.validate().is_err());
    }

    #[test]
    fn test_builder_validation_valid() {
        let builder = MessageTransportBuilder::new();
        assert!(builder.validate().is_ok());
    }

    #[test]
    fn test_builder_fluent_chain() {
        let b = MessageTransportBuilder::new()
            .recv_buffers(8)
            .send_buffers(4)
            .buffer_size(4096)
            .completion_mode(CompletionMode::Polling)
            .separate_cqs(true);
        assert_eq!(b.recv_buffer_count, 8);
        assert_eq!(b.send_buffer_count, 4);
        assert_eq!(b.buffer_size, 4096);
        assert_eq!(b.completion_mode, CompletionMode::Polling);
        assert!(b.separate_cqs);
    }

    #[test]
    fn test_derive_config() {
        let b = MessageTransportBuilder::new()
            .recv_buffers(8)
            .send_buffers(4);
        let cfg = b.derive_config();
        assert_eq!(cfg.max_send_wr, 5); // 4 + 1
        assert_eq!(cfg.max_recv_wr, 9); // 8 + 1
        assert_eq!(cfg.inflight_capacity, 14); // 5 + 9
        assert_eq!(cfg.cq_depth, 18); // 14 + 4
    }
}
