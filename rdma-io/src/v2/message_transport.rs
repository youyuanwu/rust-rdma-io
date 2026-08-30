//! Message-oriented Send/Recv transport over RDMA.
//!
//! Provides [`MessageTransport`] with pre-registered reusable buffer pools,
//! message boundaries, bounded backpressure via application-level receive
//! credits, and cancellation-safe `send()`/`recv()` operations.
//!
//! # Explicit Driver Spawning
//!
//! Construction returns a `(MessageTransport, MessageTransportDriver)` pair.
//! The caller must explicitly spawn (or poll) the driver future on a Tokio
//! runtime for transport progress. Exactly one spawned task per endpoint
//! is sufficient in both shared and separate CQ modes.
//!
//! ```no_run
//! # use rdma_io::v2::*;
//! # use rdma_io::v2::message_transport::*;
//! # async fn example() -> Result<()> {
//! let (transport, driver) = MessageTransportBuilder::new()
//!     .completion_mode(CompletionMode::Readiness)
//!     .connect("192.168.1.1:7471".parse().unwrap())
//!     .await?;
//!
//! let driver_task = tokio::spawn(driver);
//! transport.ready().await?;
//! transport.send(b"hello").await?;
//! let msg = transport.recv().await?;
//! assert_eq!(msg.as_ref(), b"hello");
//! transport.close().await;
//! let driver_result = driver_task.await.expect("driver task panicked");
//! driver_result?;
//! # Ok(())
//! # }
//! ```
//!
//! # Wire Protocol
//!
//! Every RDMA message carries a [`protocol`] frame header
//! (12 bytes) followed by a type-specific payload. Three frame types exist:
//!
//! | Type | Purpose |
//! |------|---------|
//! | DATA | Application payload |
//! | CREDIT | Return receive credits to sender |
//! | HELLO | Readiness/capability exchange during connect |
//!
//! # Credit-Based Flow Control
//!
//! The sender must acquire one remote receive credit before posting each
//! DATA frame. Credits are initialized from the peer's HELLO handshake
//! (equal to the peer's data receive buffer count). A credit is returned
//! to the sender when the receiver reposts a data receive buffer (after
//! [`ReceivedMessage`] is dropped). RNR retry acts as a safety net for
//! transient races, NOT as the primary flow-control mechanism.
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
//! # Disconnect Monitoring
//!
//! The driver future internally monitors CM events. On peer disconnect or
//! CM error, it atomically closes the transport, wakes all pending
//! send/recv/credit waiters with [`Error::TransportClosed`], and initiates
//! QP/driver shutdown.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{Mutex, Notify, Semaphore, mpsc};

use crate::async_cm::AsyncCmListener;
use crate::cm::ConnParam;
use crate::wr::{SendFlags, SendWr, Sge, WrOpcode};

use super::connection::{
    CmMonitorHandle, CompletionMode, ConnectionBuilder, ConnectionConfig, ConnectionParts,
    ConnectionResources,
};
use super::driver::CqDriverHandle;
use super::error::{Error, Result, TransportError};
use super::mr::{AccessIntent, Mr};
use super::pd::Pd;
use super::protocol;
use super::shared_qp::{OpFuture, SharedQp};

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

    /// Set the number of pre-posted data receive buffers.
    ///
    /// This determines the initial remote receive credit count announced
    /// to the peer during the HELLO handshake.
    pub fn recv_buffers(mut self, count: usize) -> Self {
        self.recv_buffer_count = count;
        self
    }

    /// Set the number of reusable send buffers.
    pub fn send_buffers(mut self, count: usize) -> Self {
        self.send_buffer_count = count;
        self
    }

    /// Set the maximum message payload size in bytes.
    ///
    /// Backing MRs are allocated with additional space for the protocol
    /// header; the configured `buffer_size` is the exact maximum payload
    /// the application can send or receive.
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
    /// Default: derived from send + recv + control buffer counts.
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
        let ctrl_send_headroom = protocol::CTRL_SEND_COUNT;
        let ctrl_recv_count = protocol::CTRL_RECV_COUNT;
        // +1 headroom on each direction for the HELLO frame during handshake
        let max_send_wr = self.send_buffer_count + ctrl_send_headroom + 1;
        let max_recv_wr = self.recv_buffer_count + ctrl_recv_count + 1;
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
    /// Returns a `(MessageTransport, MessageTransportDriver)` pair. The
    /// caller must spawn or poll the driver for transport progress.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] on invalid builder parameters, or
    /// any verbs/CM error during resource allocation and connection setup.
    /// HELLO handshake errors are reported through the driver result and
    /// [`MessageTransport::ready()`], not from this method.
    pub async fn connect(
        self,
        addr: SocketAddr,
    ) -> Result<(MessageTransport, MessageTransportDriver)> {
        self.validate()?;
        let send_count = self.send_buffer_count;
        let recv_count = self.recv_buffer_count;
        let buf_size = self.buffer_size;
        let config = self.derive_config();

        // Data MR size includes protocol header overhead
        let data_mr_size = protocol::data_mr_size(buf_size);

        // Pre-allocated recv state: posted inside pre_establish callback
        let pre_posted: std::sync::Mutex<Option<Vec<OpFuture>>> = std::sync::Mutex::new(None);

        let builder = ConnectionBuilder::new(config)?;
        let parts = builder
            .connect(&addr, |sqp, pd| {
                let futures = allocate_and_prepost_recvs(
                    sqp.qp(),
                    sqp.recv_handle(),
                    pd,
                    recv_count + protocol::CTRL_RECV_COUNT, // data + control
                    data_mr_size,
                )?;
                *pre_posted.lock().unwrap() = Some(futures);
                Ok(())
            })
            .await?;

        let pre_posted = pre_posted
            .into_inner()
            .unwrap()
            .ok_or_else(|| Error::InvalidConfig("pre_establish not called".into()))?;

        MessageTransport::from_parts(parts, send_count, recv_count, buf_size, pre_posted).await
    }

    /// Accept a connection and create a transport (server side).
    ///
    /// Returns a `(MessageTransport, MessageTransportDriver)` pair. The
    /// caller must spawn or poll the driver for transport progress.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] on invalid builder parameters, or
    /// any verbs/CM error during resource allocation and connection setup.
    /// HELLO handshake errors are reported through the driver result and
    /// [`MessageTransport::ready()`], not from this method.
    pub async fn accept(
        self,
        listener: &AsyncCmListener,
    ) -> Result<(MessageTransport, MessageTransportDriver)> {
        self.validate()?;
        let send_count = self.send_buffer_count;
        let recv_count = self.recv_buffer_count;
        let buf_size = self.buffer_size;
        let config = self.derive_config();

        let data_mr_size = protocol::data_mr_size(buf_size);

        let pre_posted: std::sync::Mutex<Option<Vec<OpFuture>>> = std::sync::Mutex::new(None);

        let builder = ConnectionBuilder::new(config)?;
        let parts = builder
            .accept(listener, |sqp, pd| {
                let futures = allocate_and_prepost_recvs(
                    sqp.qp(),
                    sqp.recv_handle(),
                    pd,
                    recv_count + protocol::CTRL_RECV_COUNT,
                    data_mr_size,
                )?;
                *pre_posted.lock().unwrap() = Some(futures);
                Ok(())
            })
            .await?;

        let pre_posted = pre_posted
            .into_inner()
            .unwrap()
            .ok_or_else(|| Error::InvalidConfig("pre_establish not called".into()))?;

        MessageTransport::from_parts(parts, send_count, recv_count, buf_size, pre_posted).await
    }
}

/// Allocate receive MRs and post them as recv WRs before the CM handshake.
fn allocate_and_prepost_recvs(
    qp: &Arc<super::qp::Qp>,
    handle: &Arc<CqDriverHandle>,
    pd: &Pd,
    count: usize,
    mr_size: usize,
) -> Result<Vec<OpFuture>> {
    let mut futures = Vec::with_capacity(count);
    for _ in 0..count {
        let mr = pd.reg_mr(mr_size, AccessIntent::LocalOnly)?;
        let future = post_recv_and_track(qp, handle, mr)?;
        futures.push(future);
    }
    Ok(futures)
}

/// A received message with its exact byte length.
///
/// Wraps a registered MR and exposes only the received payload (after
/// the protocol header). When dropped, the backing MR is returned to
/// the transport's receive pool for reposting, which also sends a
/// CREDIT frame to the peer.
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
    /// The received payload byte length (excluding protocol header).
    pub fn len(&self) -> usize {
        self.byte_len
    }

    /// Whether the received message is empty (zero-length payload).
    pub fn is_empty(&self) -> bool {
        self.byte_len == 0
    }
}

impl AsRef<[u8]> for ReceivedMessage {
    fn as_ref(&self) -> &[u8] {
        let mr = self.mr.as_ref().expect("ReceivedMessage used after take");
        let start = protocol::HEADER_SIZE;
        &mr.as_slice()[start..start + self.byte_len]
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
            // Return MR to recv pump for reposting + credit return.
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

// ═══════════════════════════════════════════════════════════════════════════
// Transport Shared State
// ═══════════════════════════════════════════════════════════════════════════

/// Transport lifecycle states.
const STATE_CREATED: u8 = 0;
const STATE_READY: u8 = 2;
const STATE_CLOSING: u8 = 3;
const STATE_STOPPED: u8 = 4;
const STATE_FAILED: u8 = 5;

/// Shared lifecycle state between [`MessageTransport`] and [`MessageTransportDriver`].
pub(crate) struct TransportSharedState {
    /// Lifecycle state machine.
    pub(crate) state: AtomicU8,
    /// Wakes `ready()`, `send()`, `recv()`, `close()` on state changes.
    pub(crate) state_notify: Notify,
    /// Remote receive credits (initialized empty, filled by driver after HELLO).
    pub(crate) remote_credits: Semaphore,
    /// Whether the frontend is still alive.
    pub(crate) frontend_alive: AtomicBool,
    /// Driver handle refs for shutdown.
    pub(crate) driver_handles: Vec<Arc<CqDriverHandle>>,
    /// Shared QP for send operations in both frontend and driver.
    pub(crate) shared_qp: Arc<SharedQp>,
    /// Terminal error snapshot (stored once, readable from frontend).
    pub(crate) error: std::sync::Mutex<Option<TransportError>>,
}

impl TransportSharedState {
    fn new(shared_qp: Arc<SharedQp>, driver_handles: Vec<Arc<CqDriverHandle>>) -> Self {
        Self {
            state: AtomicU8::new(STATE_CREATED),
            state_notify: Notify::new(),
            remote_credits: Semaphore::new(0),
            frontend_alive: AtomicBool::new(true),
            driver_handles,
            shared_qp,
            error: std::sync::Mutex::new(None),
        }
    }

    fn is_terminal(&self) -> bool {
        let s = self.state.load(Ordering::Acquire);
        s == STATE_STOPPED || s == STATE_FAILED
    }

    /// Store a terminal error snapshot (once — first error wins).
    fn store_error(&self, error: &Error) {
        let mut guard = self.error.lock().unwrap();
        if guard.is_none() {
            *guard = Some(TransportError::from_error(error));
        }
    }

    /// Mark the driver as dead and wake all waiters.
    ///
    /// Idempotent: does not overwrite a terminal state.
    fn mark_driver_dead(&self) {
        // Try to transition to Failed; if already terminal, don't overwrite
        let current = self.state.load(Ordering::Acquire);
        if current != STATE_STOPPED && current != STATE_FAILED {
            self.state.store(STATE_FAILED, Ordering::Release);
        }
        self.remote_credits.close();
        self.state_notify.notify_waiters();

        // Shut down the QP and CQ drivers
        let _ = self.shared_qp.shutdown();
        for h in &self.driver_handles {
            h.flush_and_shutdown();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Message Transport Driver
// ═══════════════════════════════════════════════════════════════════════════

/// The driver future for a message transport.
///
/// Owns all background processing: CQ completion driving, receive
/// pumping, HELLO/credit protocol, CM disconnect monitoring, and
/// cancellation reclamation. Implements `Future<Output = Result<()>> + Send`.
///
/// The caller must spawn this on a Tokio runtime for transport progress.
/// Exactly one spawned task per endpoint is sufficient in both shared
/// and separate CQ modes.
///
/// # Drop
///
/// Dropping the driver (whether never polled, or aborted mid-flight)
/// transitions the transport to failed state and wakes all frontend
/// waiters. This is safe and deterministic.
pub struct MessageTransportDriver {
    inner: Option<Pin<Box<dyn Future<Output = Result<()>> + Send>>>,
    state: Arc<TransportSharedState>,
}

impl Future for MessageTransportDriver {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self
            .inner
            .as_mut()
            .expect("MessageTransportDriver polled after completion");
        match inner.as_mut().poll(cx) {
            Poll::Ready(result) => {
                self.inner = None; // prevent double-poll
                if let Err(ref e) = result {
                    self.state.store_error(e);
                    self.state.mark_driver_dead();
                } else {
                    // Normal completion — mark stopped
                    let current = self.state.state.load(Ordering::Acquire);
                    if current != STATE_FAILED {
                        self.state.state.store(STATE_STOPPED, Ordering::Release);
                    }
                    self.state.remote_credits.close();
                    self.state.state_notify.notify_waiters();
                }
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for MessageTransportDriver {
    fn drop(&mut self) {
        // If inner future still exists, the driver was dropped before
        // completing (never polled or aborted mid-flight).
        if self.inner.is_some() {
            self.state.store_error(&Error::DriverShutdown);
        }
        // Synchronous cleanup: mark driver dead, close credits,
        // wake all waiters — runs even if never polled or aborted
        self.state.mark_driver_dead();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Internal: Completed Receive
// ═══════════════════════════════════════════════════════════════════════════

/// Internal struct for a completed receive to pass through the channel.
struct CompletedRecv {
    mr: Mr,
    byte_len: usize,
}

/// Message-oriented RDMA transport (frontend handle).
///
/// Provides async `send()` and `recv()` with pre-registered reusable
/// buffer pools, message boundaries, credit-based flow control, and
/// deterministic lifecycle management.
///
/// This is the frontend half of the two-object contract. The driver
/// future ([`MessageTransportDriver`]) must be spawned separately for
/// transport progress.
///
/// # Task Count
///
/// Exactly one user-spawned driver task per endpoint is sufficient
/// in both shared and separate CQ modes. `send()` and `recv()` run
/// in the caller's task context.
///
/// # Cancellation Safety
///
/// - Cancelling `send()` before WR posting: the credit permit is returned
///   automatically. No resource leak.
/// - Cancelling `send()` after WR posting: the MR returns to the send pool
///   when the CQE arrives via `on_cancel_reclaim`.
/// - Cancelling `recv()`: the message stays in the internal channel
///   for the next `recv()` call. No message is lost.
pub struct MessageTransport {
    buffer_size: usize,
    /// Send pool: bounded channel of available send MRs.
    send_pool_tx: mpsc::Sender<Mr>,
    send_pool_rx: Arc<Mutex<mpsc::Receiver<Mr>>>,
    /// Receive message channel: completed receives waiting for recv().
    recv_msg_rx: Arc<Mutex<mpsc::Receiver<CompletedRecv>>>,
    /// Repost channel: MRs returned from ReceivedMessage for reposting.
    repost_tx: mpsc::UnboundedSender<Mr>,
    /// Shared lifecycle state with the driver.
    state: Arc<TransportSharedState>,
}

impl MessageTransport {
    /// Build a transport pair from established connection parts.
    async fn from_parts(
        mut parts: ConnectionParts,
        send_count: usize,
        recv_count: usize,
        buffer_size: usize,
        pre_posted_recvs: Vec<OpFuture>,
    ) -> Result<(Self, MessageTransportDriver)> {
        let pd = parts.shared_qp.pd().clone();
        let data_mr_size = protocol::data_mr_size(buffer_size);

        let shared_qp = Arc::new(parts.shared_qp);
        let driver_handles = parts.driver_handles;

        let shared_state = Arc::new(TransportSharedState::new(
            Arc::clone(&shared_qp),
            driver_handles.clone(),
        ));

        // === Allocate send buffers ===
        let (send_pool_tx, send_pool_rx) = mpsc::channel(send_count);
        for _ in 0..send_count {
            let mr = pd.reg_mr(data_mr_size, AccessIntent::LocalOnly)?;
            send_pool_tx
                .send(mr)
                .await
                .map_err(|_| Error::InvalidConfig("send pool channel closed".into()))?;
        }

        // === Allocate control send MRs ===
        let (ctrl_send_tx, ctrl_send_rx) = mpsc::channel(protocol::CTRL_SEND_COUNT);
        for _ in 0..protocol::CTRL_SEND_COUNT {
            let mr = pd.reg_mr(protocol::CTRL_BUF_SIZE, AccessIntent::LocalOnly)?;
            ctrl_send_tx
                .send(mr)
                .await
                .map_err(|_| Error::InvalidConfig("ctrl send pool channel closed".into()))?;
        }

        // Receive channels
        let (recv_msg_tx, recv_msg_rx) = mpsc::channel(recv_count + protocol::CTRL_RECV_COUNT);
        let (repost_tx, repost_rx) = mpsc::unbounded_channel();

        let cm_monitor_handle = parts.cm_monitor_handle.take();
        let recv_handle_idx = if driver_handles.len() > 1 { 1 } else { 0 };
        let driver_recv_handle = driver_handles[recv_handle_idx].clone();
        let driver_send_handle = driver_handles[0].clone();

        // Store SharedQp Arc in resources for proper drop ordering:
        // SharedQp (QP destroy) must happen before CmId destroy.
        let mut resources = parts.resources;
        resources.shared_qp = Some(Arc::clone(&shared_qp));

        // NOTE: Do NOT clone send_pool_tx or repost_tx into the driver.
        // The driver must not hold these senders — when the frontend drops,
        // the channel closures signal the driver to shut down.
        let driver_future: Pin<Box<dyn Future<Output = Result<()>> + Send>> = Box::pin(driver_run(
            Arc::clone(&shared_state),
            Arc::clone(&shared_qp),
            driver_send_handle,
            driver_recv_handle,
            driver_handles.clone(),
            parts.driver_futures,
            resources,
            cm_monitor_handle,
            pd,
            pre_posted_recvs,
            recv_count,
            buffer_size,
            recv_msg_tx,
            repost_rx,
            ctrl_send_tx,
            ctrl_send_rx,
        ));

        let transport = Self {
            buffer_size,
            send_pool_tx,
            send_pool_rx: Arc::new(Mutex::new(send_pool_rx)),
            recv_msg_rx: Arc::new(Mutex::new(recv_msg_rx)),
            repost_tx,
            state: Arc::clone(&shared_state),
        };

        let driver = MessageTransportDriver {
            inner: Some(driver_future),
            state: shared_state,
        };

        Ok((transport, driver))
    }

    /// Wait for the transport to become ready (HELLO handshake complete).
    pub async fn ready(&self) -> Result<()> {
        loop {
            let notified = self.state.state_notify.notified();
            let s = self.state.state.load(Ordering::Acquire);
            match s {
                STATE_READY => return Ok(()),
                STATE_FAILED => return Err(self.terminal_error()),
                STATE_CLOSING | STATE_STOPPED => return Err(Error::TransportClosed),
                _ => {}
            }
            notified.await;
        }
    }

    /// Wait for readiness internally.
    async fn await_ready(&self) -> Result<()> {
        let s = self.state.state.load(Ordering::Acquire);
        if s == STATE_READY {
            return Ok(());
        }
        if s == STATE_FAILED {
            return Err(self.terminal_error());
        }
        if s >= STATE_CLOSING {
            return Err(Error::TransportClosed);
        }
        self.ready().await
    }

    /// Access the shared queue pair.
    pub fn shared_qp(&self) -> &SharedQp {
        &self.state.shared_qp
    }

    /// Access the driver handles.
    pub fn driver_handles(&self) -> &[Arc<CqDriverHandle>] {
        &self.state.driver_handles
    }

    /// Inspect the terminal driver error, if any.
    ///
    /// Returns `Some(TransportError)` if the driver exited with an error
    /// (e.g., HELLO timeout, completion error, driver drop/abort).
    /// Returns `None` if the transport was cleanly closed, is still running,
    /// or peer-disconnected without a driver-level error.
    ///
    /// The returned [`TransportError`] carries the same cause information
    /// as the driver future's `Result<()>` output, providing two
    /// observation channels for the same terminal event.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use rdma_io::v2::*;
    /// # use rdma_io::v2::error::TransportErrorKind;
    /// # async fn example(transport: MessageTransport, driver_handle: tokio::task::JoinHandle<Result<()>>) {
    /// // After driver completes:
    /// let driver_result = driver_handle.await.expect("panicked");
    /// if let Err(ref e) = driver_result {
    ///     // Both channels observe the same cause:
    ///     let frontend_err = transport.error().expect("error should be set");
    ///     eprintln!("driver: {e}, frontend: {frontend_err}");
    /// }
    /// # }
    /// ```
    pub fn error(&self) -> Option<TransportError> {
        self.state.error.lock().unwrap().clone()
    }

    /// Return the stored terminal error (if any) or fall back to
    /// [`Error::TransportClosed`].
    ///
    /// Used by `ready()`, `send()`, `recv()` to surface actionable
    /// error information rather than an opaque `TransportClosed`.
    fn terminal_error(&self) -> Error {
        if let Some(te) = self.state.error.lock().unwrap().clone() {
            Error::TransportFailed(te)
        } else {
            Error::TransportClosed
        }
    }

    /// Send a message. Returns when the local send completion arrives.
    ///
    /// # Errors
    ///
    /// - [`Error::MessageTooLarge`] if `data.len() > buffer_size`
    /// - [`Error::TransportClosed`] if the transport is shut down or disconnected
    /// - [`Error::CompletionError`] if the send WR completed with error
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        if data.len() > self.buffer_size {
            return Err(Error::MessageTooLarge {
                size: data.len(),
                capacity: self.buffer_size,
            });
        }

        // Await readiness (HELLO must be complete)
        self.await_ready().await?;

        // Acquire remote receive credit
        let credit_permit = self
            .state
            .remote_credits
            .acquire()
            .await
            .map_err(|_| self.terminal_error())?;

        // Acquire send buffer from the pool
        let mut mr = {
            let mut rx = self.send_pool_rx.lock().await;
            rx.recv().await.ok_or_else(|| self.terminal_error())?
        };

        // === SYNCHRONOUS SECTION ===
        let frame_len = protocol::write_data_frame(mr.as_mut_slice(), data);
        let qp = self.state.shared_qp.qp().clone();
        let handle = self.state.shared_qp.send_handle().clone();

        let reg = match handle.map().register() {
            Some(r) => r,
            None => {
                let _ = self.send_pool_tx.try_send(mr);
                return Err(Error::CapacityExhausted);
            }
        };
        let token = reg.token;

        let addr = mr.addr();
        let sge = Sge::new(addr, frame_len as u32, mr.lkey());
        let mut wr = SendWr::new(token, WrOpcode::Send)
            .sg(sge)
            .flags(SendFlags::SIGNALED);

        if let Err(e) = qp.post_send_wr_raw(&mut wr) {
            handle.map().release(token);
            let _ = self.send_pool_tx.try_send(mr);
            return Err(e);
        }
        handle.notify_work();
        credit_permit.forget();
        // === END SYNCHRONOUS SECTION ===

        let send_pool_tx = self.send_pool_tx.clone();
        let op = OpFuture::new_inflight(handle, token, mr).on_cancel_reclaim(Box::new(move |mr| {
            let _ = send_pool_tx.try_send(mr);
        }));

        let (result, mr) = op.await;
        let _ = self.send_pool_tx.try_send(mr);

        result.map_err(|e| match &e {
            Error::CompletionError { status, .. } if *status == crate::wc::WcStatus::WrFlushErr => {
                Error::TransportClosed
            }
            _ => e,
        })?;

        Ok(())
    }

    /// Receive a message. Returns the next completed message.
    ///
    /// # Errors
    ///
    /// - [`Error::TransportClosed`] if the transport is shut down or disconnected
    pub async fn recv(&self) -> Result<ReceivedMessage> {
        self.await_ready().await?;

        let completed = {
            let mut rx = self.recv_msg_rx.lock().await;
            rx.recv().await.ok_or_else(|| self.terminal_error())?
        };

        Ok(ReceivedMessage {
            mr: Some(completed.mr),
            byte_len: completed.byte_len,
            repost_tx: self.repost_tx.clone(),
        })
    }

    /// The configured maximum message payload size.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Graceful async shutdown.
    ///
    /// Signals the driver to shut down and waits for it to reach
    /// a terminal state. Does NOT own the driver's `JoinHandle` —
    /// the caller is responsible for awaiting the spawn handle.
    ///
    /// Returns immediately if the driver has already stopped (including
    /// if the driver was never spawned and was dropped).
    pub async fn close(&self) {
        // Try to transition to Closing atomically
        let _ = self.state.state.compare_exchange(
            STATE_CREATED,
            STATE_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let _ = self.state.state.compare_exchange(
            STATE_READY,
            STATE_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.state.remote_credits.close();
        self.state.state_notify.notify_waiters();

        // Wait for terminal state (or return immediately if already terminal)
        loop {
            let notified = self.state.state_notify.notified();
            if self.state.is_terminal() {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for MessageTransport {
    fn drop(&mut self) {
        // Signal frontend absence to the driver
        self.state.frontend_alive.store(false, Ordering::Release);
        self.state.remote_credits.close();
        self.state.state_notify.notify_waiters();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Composed Driver Future
// ═══════════════════════════════════════════════════════════════════════════

/// The composed driver async function — runs CQ drivers, HELLO handshake,
/// recv pump, and disconnect monitor all within one task.
///
/// # Phases
///
/// - **Phase A (Handshake)**: CQ drivers poll concurrently with HELLO
///   send/receive. Once HELLO succeeds, credits are initialized and state
///   transitions to Ready.
/// - **Phase B (Steady State)**: CQ drivers, recv pump, and disconnect
///   monitor run concurrently in a select loop.
/// - **Phase C (Shutdown)**: On close/disconnect/error, the QP is
///   transitioned to error, CQ drivers drain, then the function exits.
#[expect(clippy::too_many_arguments)]
async fn driver_run(
    state: Arc<TransportSharedState>,
    shared_qp: Arc<SharedQp>,
    send_handle: Arc<CqDriverHandle>,
    recv_handle: Arc<CqDriverHandle>,
    driver_handles: Vec<Arc<CqDriverHandle>>,
    cq_driver_futures: Vec<Pin<Box<dyn Future<Output = super::error::Result<()>> + Send>>>,
    mut _resources: ConnectionResources, // kept alive for drop ordering
    cm_monitor_handle: Option<CmMonitorHandle>,
    _pd: Pd, // kept alive so MRs remain valid
    initial_recvs: Vec<OpFuture>,
    recv_count: usize,
    buffer_size: usize,
    recv_msg_tx: mpsc::Sender<CompletedRecv>,
    mut repost_rx: mpsc::UnboundedReceiver<Mr>,
    ctrl_send_tx: mpsc::Sender<Mr>,
    mut ctrl_send_rx: mpsc::Receiver<Mr>,
) -> Result<()> {
    use crate::cm::CmEventType;

    // Combine all CQ driver futures into one joined future.
    // We use a FuturesUnordered to poll them all concurrently.
    let mut cq_futures: futures_util::stream::FuturesUnordered<_> =
        cq_driver_futures.into_iter().collect();

    // ─── Phase A: HELLO Handshake (concurrent with CQ drivers) ───

    // Send our HELLO frame
    let mut hello_mr = shared_qp
        .pd()
        .reg_mr(protocol::HELLO_FRAME_SIZE, AccessIntent::LocalOnly)?;
    let hello_len = protocol::write_hello_frame(
        hello_mr.as_mut_slice(),
        recv_count as u32,
        buffer_size as u32,
    );

    // We need to poll CQ drivers concurrently with HELLO send + recv
    // because the OpFutures require CQ dispatch to complete.
    let mut remaining_futures = initial_recvs;

    // Send HELLO — this creates an OpFuture that needs CQ driver to dispatch
    let hello_send = shared_qp.send(hello_mr, Some((0, hello_len)));
    tokio::pin!(hello_send);

    // Phase A loop: drive CQ futures while waiting for HELLO send
    let mut hello_sent = false;
    let mut peer_capacity: Option<usize> = None;

    let hello_timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(hello_timeout);

    loop {
        // Check if we're done with handshake
        if hello_sent && peer_capacity.is_some() {
            break;
        }

        if !state.frontend_alive.load(Ordering::Acquire) {
            return Err(Error::TransportClosed);
        }

        tokio::select! {
            biased;

            // Drive CQ completions (always)
            Some(cq_result) = futures_util::StreamExt::next(&mut cq_futures) => {
                // A CQ driver exited during handshake — this is a problem
                match cq_result {
                    Ok(()) => return Err(Error::DriverShutdown),
                    Err(e) => return Err(e),
                }
            }

            // Drive HELLO send if not done
            result = &mut hello_send, if !hello_sent => {
                let (send_result, _mr) = result;
                send_result?;
                hello_sent = true;
            }

            // Drive recv completions to catch peer's HELLO
            result = poll_any_ready(&mut remaining_futures), if peer_capacity.is_none() && !remaining_futures.is_empty() => {
                match result {
                    PollResult::Ready(idx, Ok((completion, mr))) => {
                        remaining_futures.swap_remove(idx);
                        let byte_len = completion.byte_len() as usize;
                        let header = protocol::parse_header(mr.as_slice(), byte_len)?;
                        if header.frame_type == protocol::FRAME_HELLO {
                            let hello = protocol::parse_hello(&mr.as_slice()[protocol::HEADER_SIZE..])?;
                            let peer_max = hello.max_message_size as usize;
                            if peer_max < buffer_size {
                                return Err(Error::ProtocolViolation(format!(
                                    "peer max_message_size {peer_max} < local buffer_size {buffer_size}; \
                                     sending max-size messages would overrun peer recv buffers"
                                )));
                            }
                            peer_capacity = Some(hello.data_recv_capacity as usize);
                            // Repost the HELLO recv buffer
                            let qp = shared_qp.qp().clone();
                            let future = post_recv_and_track(&qp, &recv_handle, mr)?;
                            remaining_futures.push(future);
                        } else {
                            return Err(Error::ProtocolViolation(format!(
                                "expected HELLO, got frame_type={}",
                                header.frame_type,
                            )));
                        }
                    }
                    PollResult::Ready(idx, Err((err, _mr))) => {
                        remaining_futures.swap_remove(idx);
                        return Err(err);
                    }
                }
            }

            // Timeout
            () = &mut hello_timeout => {
                return Err(Error::ProtocolViolation("HELLO handshake timeout".into()));
            }
        }
    }

    // Initialize credits from peer capacity
    let credits = peer_capacity.unwrap();
    state.remote_credits.add_permits(credits);

    // Transition to Ready (use compare_exchange to detect concurrent close)
    if state
        .state
        .compare_exchange(
            STATE_CREATED,
            STATE_READY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        // State was changed (e.g., to CLOSING) during handshake — exit
        return Ok(());
    }
    state.state_notify.notify_waiters();

    // ─── Phase B: Steady State (recv pump + disconnect monitor + CQ drivers) ───

    let mut pending_recvs = remaining_futures;
    let mut pending_credits: u32 = 0;
    let mut phase_b_error: Option<Error> = None;

    // Set up disconnect monitor as a pinned future
    let disconnect_future = async {
        if let Some(handle) = cm_monitor_handle {
            loop {
                let guard_result = handle.cm_async_fd.readable().await;
                let mut guard = match guard_result {
                    Ok(g) => g,
                    Err(_) => break,
                };

                match handle.event_channel.try_get_event() {
                    Ok(event) => {
                        let event_type = event.event_type();
                        tracing::debug!(?event_type, "driver: CM event");
                        match event_type {
                            CmEventType::Disconnected | CmEventType::DeviceRemoval => break,
                            _ => {}
                        }
                    }
                    Err(crate::Error::WouldBlock) => {
                        guard.clear_ready();
                        continue;
                    }
                    Err(_) => break,
                }

                guard.clear_ready();
            }
        } else {
            // No CM handle — just pend forever
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(disconnect_future);

    loop {
        // Check state: closing, frontend dropped, etc.
        let current_state = state.state.load(Ordering::Acquire);
        if current_state >= STATE_CLOSING {
            break;
        }
        if !state.frontend_alive.load(Ordering::Acquire) {
            break;
        }

        // Register shutdown interest BEFORE entering select (avoids lost wakeup)
        let shutdown_notified = state.state_notify.notified();
        tokio::pin!(shutdown_notified);

        // Re-check after registering (close/drop may have fired between check and register)
        let current_state = state.state.load(Ordering::Acquire);
        if current_state >= STATE_CLOSING || !state.frontend_alive.load(Ordering::Acquire) {
            break;
        }

        // Flush pending credits if we have a control MR available
        if pending_credits > 0
            && let Ok(mut ctrl_mr) = ctrl_send_rx.try_recv()
        {
            let credits_to_send = pending_credits;
            pending_credits = 0;
            let frame_len = protocol::write_credit_frame(ctrl_mr.as_mut_slice(), credits_to_send);
            let ctx = ctrl_send_tx.clone();
            let _ = post_send_and_detach(
                shared_qp.qp(),
                &send_handle,
                ctrl_mr,
                frame_len,
                Box::new(move |mr| {
                    let _ = ctx.try_send(mr);
                }),
            );
        }

        if pending_recvs.is_empty() {
            // All buffers are out — wait for repost, ctrl MR, CQ, or shutdown
            tokio::select! {
                biased;

                // Shutdown signal (close/drop)
                () = &mut shutdown_notified => {
                    continue; // re-evaluates loop-head checks
                }

                // CQ driver exit
                Some(cq_result) = futures_util::StreamExt::next(&mut cq_futures) => {
                    if let Err(e) = cq_result {
                        tracing::warn!("driver: CQ driver error: {e}");
                        phase_b_error = Some(e);
                    }
                    break;
                }

                // Disconnect
                () = &mut disconnect_future => {
                    tracing::info!("driver: peer disconnected");
                    break;
                }

                mr = repost_rx.recv() => {
                    match mr {
                        Some(mr) => {
                            pending_credits += 1;
                            match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                Ok(future) => pending_recvs.push(future),
                                Err(_) => break,
                            }
                        }
                        None => break, // frontend dropped repost sender
                    }
                    continue;
                }

                ctrl_mr = ctrl_send_rx.recv(), if pending_credits > 0 => {
                    if let Some(mut ctrl_mr) = ctrl_mr {
                        let credits_to_send = pending_credits;
                        pending_credits = 0;
                        let frame_len = protocol::write_credit_frame(ctrl_mr.as_mut_slice(), credits_to_send);
                        let ctx = ctrl_send_tx.clone();
                        let _ = post_send_and_detach(shared_qp.qp(), &send_handle, ctrl_mr, frame_len, Box::new(move |mr| { let _ = ctx.try_send(mr); }));
                    }
                    continue;
                }
            }
        } else {
            // Normal operation: poll recv completions, reposts, CQ, disconnect
            tokio::select! {
                biased;

                // Shutdown signal (close/drop)
                () = &mut shutdown_notified => {
                    continue; // re-evaluates loop-head checks
                }

                // CQ driver exit
                Some(cq_result) = futures_util::StreamExt::next(&mut cq_futures) => {
                    if let Err(e) = cq_result {
                        tracing::warn!("driver: CQ driver error: {e}");
                        phase_b_error = Some(e);
                    }
                    break;
                }

                // Disconnect
                () = &mut disconnect_future => {
                    tracing::info!("driver: peer disconnected");
                    break;
                }

                // Recv completion
                result = poll_any_ready(&mut pending_recvs) => {
                    match result {
                        PollResult::Ready(idx, Ok((completion, mr))) => {
                            pending_recvs.swap_remove(idx);
                            let byte_len = completion.byte_len() as usize;

                            match protocol::parse_header(mr.as_slice(), byte_len) {
                                Ok(header) => {
                                    match header.frame_type {
                                        protocol::FRAME_DATA => {
                                            let payload_len = header.payload_len as usize;
                                            if recv_msg_tx.send(CompletedRecv { mr, byte_len: payload_len }).await.is_err() {
                                                break; // recv channel closed
                                            }
                                        }
                                        protocol::FRAME_CREDIT => {
                                            if let Ok(credit) = protocol::parse_credit(&mr.as_slice()[protocol::HEADER_SIZE..]) {
                                                state.remote_credits.add_permits(credit.credits as usize);
                                            }
                                            match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                                Ok(future) => pending_recvs.push(future),
                                                Err(_) => break,
                                            }
                                        }
                                        protocol::FRAME_HELLO => {
                                            tracing::warn!("driver: unexpected HELLO during normal operation");
                                            match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                                Ok(future) => pending_recvs.push(future),
                                                Err(_) => break,
                                            }
                                        }
                                        _ => {
                                            tracing::warn!(frame_type = header.frame_type, "driver: unknown frame type");
                                            match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                                Ok(future) => pending_recvs.push(future),
                                                Err(_) => break,
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("driver: protocol error: {e}");
                                    match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                        Ok(future) => pending_recvs.push(future),
                                        Err(_) => break,
                                    }
                                }
                            }
                        }
                        PollResult::Ready(idx, Err((err, _mr))) => {
                            pending_recvs.swap_remove(idx);
                            tracing::debug!("driver: recv completion error: {err}, shutting down");
                            phase_b_error = Some(err);
                            pending_recvs.clear();
                            break;
                        }
                    }
                }

                // Drain repost channel eagerly
                mr = repost_rx.recv() => {
                    match mr {
                        Some(mr) => {
                            pending_credits += 1;
                            match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                Ok(future) => pending_recvs.push(future),
                                Err(_) => break,
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    }

    // ─── Phase C: Shutdown ───

    // Close credits first to unblock any waiting frontend operations
    state.remote_credits.close();

    // Initiate QP shutdown — transitions QP to error, generating flush CQEs
    let _ = shared_qp.shutdown();
    for h in &driver_handles {
        h.flush_and_shutdown();
    }

    // Drop pending recv OpFutures BEFORE CQ drain — releases inflight
    // registry slots so the drain barrier can reach zero
    drop(pending_recvs);
    drop(recv_msg_tx);

    // Let CQ drivers drain their final barrier
    let drain_timeout = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(drain_timeout);

    loop {
        tokio::select! {
            biased;
            Some(_) = futures_util::StreamExt::next(&mut cq_futures) => {
                // CQ driver finished
            }
            () = &mut drain_timeout => {
                tracing::warn!("driver: CQ drain timeout");
                break;
            }
        }
        if cq_futures.is_empty() {
            break;
        }
    }

    // Now transition to terminal state and notify waiters
    // (AFTER drain is complete, so close().await means drain is done)
    if let Some(ref err) = phase_b_error {
        // Store the error for frontend inspection before transitioning state
        state.store_error(err);
        let current = state.state.load(Ordering::Acquire);
        if current != STATE_FAILED {
            state.state.store(STATE_FAILED, Ordering::Release);
        }
    } else {
        let current = state.state.load(Ordering::Acquire);
        if current != STATE_FAILED {
            state.state.store(STATE_STOPPED, Ordering::Release);
        }
    }
    state.state_notify.notify_waiters();

    // Drop the SharedQp Arc from resources to ensure QP is destroyed
    // before CmId (field ordering in ConnectionResources)
    _resources.shared_qp = None;

    if let Some(err) = phase_b_error {
        Err(err)
    } else {
        Ok(())
    }
}

/// Result of polling pending recv futures.
enum PollResult {
    /// Future at index completed with the given result.
    Ready(
        usize,
        std::result::Result<(super::op::Completion, Mr), (Error, Mr)>,
    ),
}

/// Poll all pending recv futures. Returns when the first one is ready.
///
/// Properly parks the task (returns `Poll::Pending`) when no OpFuture
/// has completed, relying on the wakers registered by each OpFuture.
async fn poll_any_ready(pending: &mut [OpFuture]) -> PollResult {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Poll;

    std::future::poll_fn(|cx| {
        for (i, fut) in pending.iter_mut().enumerate() {
            let pinned = Pin::new(fut);
            if let Poll::Ready((result, mr)) = pinned.poll(cx) {
                let mapped = match result {
                    Ok(completion) => Ok((completion, mr)),
                    Err(e) => Err((e, mr)),
                };
                return Poll::Ready(PollResult::Ready(i, mapped));
            }
        }
        Poll::Pending
    })
    .await
}

/// Post a receive WR directly and return an already-inflight OpFuture.
fn post_recv_and_track(
    qp: &Arc<super::qp::Qp>,
    handle: &Arc<CqDriverHandle>,
    mr: Mr,
) -> Result<OpFuture> {
    let len = mr.len();

    let reg = handle.map().register().ok_or(Error::CapacityExhausted)?;
    let token = reg.token;

    let addr = mr.addr();
    let sge = Sge::new(addr, len as u32, mr.lkey());
    let mut wr = crate::wr::RecvWr::new(token).sg(sge);

    if let Err(e) = qp.post_recv_wr_raw(&mut wr) {
        handle.map().release(token);
        return Err(e);
    }
    handle.notify_work();

    Ok(OpFuture::new_inflight(handle.clone(), token, mr))
}

/// Post a send WR and immediately detach (fire-and-forget).
///
/// The `on_reclaim` callback is invoked when the CQE arrives, returning
/// the MR for reuse. Used for control frames (CREDIT/HELLO).
fn post_send_and_detach(
    qp: &Arc<super::qp::Qp>,
    handle: &Arc<CqDriverHandle>,
    mr: Mr,
    frame_len: usize,
    on_reclaim: Box<dyn FnOnce(Mr) + Send>,
) -> Result<()> {
    let reg = handle.map().register().ok_or(Error::CapacityExhausted)?;
    let token = reg.token;

    let addr = mr.addr();
    let sge = Sge::new(addr, frame_len as u32, mr.lkey());
    let mut wr = SendWr::new(token, WrOpcode::Send)
        .sg(sge)
        .flags(SendFlags::SIGNALED);

    if let Err(e) = qp.post_send_wr_raw(&mut wr) {
        handle.map().release(token);
        // Return MR via callback on post failure
        on_reclaim(mr);
        return Err(e);
    }
    handle.notify_work();

    // Detach — driver will reclaim when CQE arrives
    handle.push_detached(token, mr, Some(on_reclaim));
    Ok(())
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
    fn test_derive_config_with_protocol_overhead() {
        let b = MessageTransportBuilder::new()
            .recv_buffers(8)
            .send_buffers(4);
        let cfg = b.derive_config();
        // max_send_wr = send_count + ctrl_send_count + 1 = 4 + 2 + 1 = 7
        assert_eq!(cfg.max_send_wr, 7);
        // max_recv_wr = recv_count + ctrl_recv_count + 1 = 8 + 2 + 1 = 11
        assert_eq!(cfg.max_recv_wr, 11);
        // total = 7 + 11 = 18; inflight = 18
        assert_eq!(cfg.inflight_capacity, 18);
    }
}
