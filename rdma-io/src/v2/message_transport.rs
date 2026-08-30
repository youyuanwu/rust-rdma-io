//! Message-oriented Send/Recv transport over RDMA.
//!
//! Provides [`MessageTransport`] with pre-registered reusable buffer pools,
//! message boundaries, bounded backpressure via application-level receive
//! credits, and cancellation-safe `send()`/`recv()` operations.
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
//! A dedicated CM event monitor task is the sole consumer of the connection's
//! CM event channel. On peer disconnect or CM error, the monitor atomically
//! closes the transport, wakes all pending send/recv/credit waiters with
//! [`Error::TransportClosed`], and initiates QP/driver shutdown.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::async_cm::AsyncCmListener;
use crate::cm::ConnParam;
use crate::wr::{SendFlags, SendWr, Sge, WrOpcode};

use super::connection::{
    CmMonitorHandle, CompletionMode, Connection, ConnectionBuilder, ConnectionConfig,
};
use super::driver::CqDriverHandle;
use super::error::{Error, Result};
use super::mr::{AccessIntent, Mr};
use super::pd::Pd;
use super::protocol;
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
    /// Allocates all MRs, posts receive buffers (before the CM handshake),
    /// exchanges HELLO with the peer, initializes credits, and returns a
    /// ready-to-use transport.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] on invalid builder parameters,
    /// [`Error::ProtocolViolation`] on HELLO handshake failure, or any
    /// verbs/CM error during resource allocation and connection setup.
    pub async fn connect(self, addr: SocketAddr) -> Result<MessageTransport> {
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
        let conn = builder
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

        MessageTransport::from_connection(conn, send_count, recv_count, buf_size, pre_posted).await
    }

    /// Accept a connection and create a transport (server side).
    ///
    /// Allocates all MRs, posts receive buffers (before the CM `accept()`),
    /// exchanges HELLO with the peer, initializes credits, and returns a
    /// ready-to-use transport.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] on invalid builder parameters,
    /// [`Error::ProtocolViolation`] on HELLO handshake failure, or any
    /// verbs/CM error during resource allocation and connection setup.
    pub async fn accept(self, listener: &AsyncCmListener) -> Result<MessageTransport> {
        self.validate()?;
        let send_count = self.send_buffer_count;
        let recv_count = self.recv_buffer_count;
        let buf_size = self.buffer_size;
        let config = self.derive_config();

        let data_mr_size = protocol::data_mr_size(buf_size);

        let pre_posted: std::sync::Mutex<Option<Vec<OpFuture>>> = std::sync::Mutex::new(None);

        let builder = ConnectionBuilder::new(config)?;
        let conn = builder
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

        MessageTransport::from_connection(conn, send_count, recv_count, buf_size, pre_posted).await
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

/// Internal struct for a completed receive to pass through the channel.
struct CompletedRecv {
    mr: Mr,
    byte_len: usize,
}

/// Message-oriented RDMA transport.
///
/// Provides async `send()` and `recv()` with pre-registered reusable
/// buffer pools, message boundaries, credit-based flow control, and
/// deterministic disconnect handling.
///
/// # Send Semantics
///
/// `send().await` means the local send completion (CQE) was received
/// with success status. It does **not** mean the remote peer has consumed
/// the message.
///
/// # Credit-Based Backpressure
///
/// Each outbound DATA frame requires one remote receive credit, acquired
/// from a semaphore initialized from the peer's HELLO handshake. When
/// all credits are consumed, `send()` waits asynchronously. Credits are
/// returned when the peer reposts a data receive buffer (after dropping
/// a [`ReceivedMessage`]). RC RNR retry is a safety net, NOT the primary
/// flow-control mechanism.
///
/// # Ordering
///
/// Message ordering is guaranteed by RC QP semantics for WRs posted
/// by a single task. Concurrent multi-task sends may interleave at QP
/// level.
///
/// # Cancellation Safety
///
/// - Cancelling `send()` before WR posting: the credit permit is returned
///   automatically. No resource leak.
/// - Cancelling `send()` after WR posting: the MR returns to the send pool
///   when the CQE arrives via `on_cancel_reclaim`. The credit is correctly
///   consumed (the WR will use a peer recv buffer).
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
    /// Remote receive credits (initialized from peer HELLO).
    remote_credits: Arc<Semaphore>,
    /// Transport closed flag — shared with all tasks.
    closed: Arc<AtomicBool>,
    /// Recv pump task handle.
    recv_pump_task: Option<JoinHandle<()>>,
    /// Disconnect monitor task handle.
    disconnect_task: Option<JoinHandle<()>>,
}

impl MessageTransport {
    /// Build a transport from an established connection, pre-posted recv
    /// futures, and send buffer configuration.
    ///
    /// Performs the HELLO readiness handshake, initializes credits, and
    /// spawns the recv pump and disconnect monitor.
    async fn from_connection(
        mut connection: Connection,
        send_count: usize,
        recv_count: usize,
        buffer_size: usize,
        pre_posted_recvs: Vec<OpFuture>,
    ) -> Result<Self> {
        let pd = connection.pd().clone();
        let data_mr_size = protocol::data_mr_size(buffer_size);

        // === HELLO Handshake ===
        // Send our HELLO frame
        let mut hello_mr = pd.reg_mr(protocol::HELLO_FRAME_SIZE, AccessIntent::LocalOnly)?;
        let hello_len = protocol::write_hello_frame(
            hello_mr.as_mut_slice(),
            recv_count as u32,
            buffer_size as u32,
        );
        let (result, _hello_mr) = connection
            .shared_qp()
            .send(hello_mr, Some((0, hello_len)))
            .await;
        result?;

        // Wait for peer's HELLO (with timeout)
        let mut remaining_futures = pre_posted_recvs;
        let peer_capacity = tokio::time::timeout(Duration::from_secs(10), async {
            let poll_result = poll_any_ready(&mut remaining_futures).await;
            match poll_result {
                PollResult::Ready(idx, Ok((completion, mr))) => {
                    remaining_futures.swap_remove(idx);
                    let byte_len = completion.byte_len() as usize;
                    let header = protocol::parse_header(mr.as_slice(), byte_len)?;
                    if header.frame_type == protocol::FRAME_HELLO {
                        let hello = protocol::parse_hello(&mr.as_slice()[protocol::HEADER_SIZE..])?;
                        // Validate peer's max_message_size is compatible
                        let peer_max = hello.max_message_size as usize;
                        if peer_max < buffer_size {
                            return Err(Error::ProtocolViolation(format!(
                                "peer max_message_size {peer_max} < local buffer_size {buffer_size}; \
                                 sending max-size messages would overrun peer recv buffers"
                            )));
                        }
                        let capacity = hello.data_recv_capacity as usize;
                        let qp = connection.shared_qp().qp().clone();
                        let handle = connection.shared_qp().recv_handle().clone();
                        let future = post_recv_and_track(&qp, &handle, mr)?;
                        remaining_futures.push(future);
                        Ok::<usize, Error>(capacity)
                    } else {
                        Err(Error::ProtocolViolation(format!(
                            "expected HELLO, got frame_type={}",
                            header.frame_type,
                        )))
                    }
                }
                PollResult::Ready(idx, Err((err, _mr))) => {
                    remaining_futures.swap_remove(idx);
                    Err(err)
                }
            }
        })
        .await
        .map_err(|_| Error::ProtocolViolation("HELLO handshake timeout".into()))??;

        // Initialize credit semaphore from peer's announced data recv capacity
        let remote_credits = Arc::new(Semaphore::new(peer_capacity));
        let closed = Arc::new(AtomicBool::new(false));

        // === Allocate send buffers (data MR size for protocol header) ===
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

        // Recv pump
        let pump_qp = connection.shared_qp().qp().clone();
        let pump_send_handle = connection.shared_qp().send_handle().clone();
        let pump_recv_handle = if connection.driver_handles().len() > 1 {
            connection.driver_handles()[1].clone()
        } else {
            connection.driver_handles()[0].clone()
        };
        let pump_credits = remote_credits.clone();
        let pump_closed = closed.clone();

        let recv_pump_task = tokio::spawn(recv_pump(
            pump_qp,
            pump_send_handle,
            pump_recv_handle,
            pd,
            remaining_futures,
            recv_msg_tx,
            repost_rx,
            ctrl_send_tx.clone(),
            ctrl_send_rx,
            pump_credits,
            pump_closed,
        ));

        // === Disconnect Monitor ===
        let cm_handle = connection.take_cm_monitor_handle();
        let disc_closed = closed.clone();
        let disc_credits = remote_credits.clone();
        let disc_handles: Vec<_> = connection.driver_handles().to_vec();

        let disconnect_task = cm_handle.map(|handle| {
            tokio::spawn(disconnect_monitor(
                handle,
                disc_closed,
                disc_credits,
                disc_handles,
            ))
        });

        Ok(Self {
            connection,
            buffer_size,
            send_pool_tx,
            send_pool_rx: Arc::new(Mutex::new(send_pool_rx)),
            recv_msg_rx: Arc::new(Mutex::new(recv_msg_rx)),
            repost_tx,
            remote_credits,
            closed,
            recv_pump_task: Some(recv_pump_task),
            disconnect_task,
        })
    }

    /// Send a message. Returns when the local send completion arrives.
    ///
    /// `send().await` is local-completion only — it does NOT guarantee
    /// remote consumption.
    ///
    /// # Credit Flow
    ///
    /// Acquires one remote receive credit before posting. If no credits
    /// are available, waits asynchronously until the peer returns credits
    /// (by dropping received messages).
    ///
    /// # Errors
    ///
    /// - [`Error::MessageTooLarge`] if `data.len() > buffer_size`
    /// - [`Error::TransportClosed`] if the transport is shut down or
    ///   disconnected (including credit semaphore closed)
    /// - [`Error::CompletionError`] if the send WR completed with error
    ///
    /// # Cancellation Safety
    ///
    /// If cancelled before the WR is posted, the credit permit is returned
    /// automatically. If cancelled after posting, the `on_cancel_reclaim`
    /// callback returns the MR to the send pool when the CQE arrives.
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        if data.len() > self.buffer_size {
            return Err(Error::MessageTooLarge {
                size: data.len(),
                capacity: self.buffer_size,
            });
        }

        if self.closed.load(Ordering::Acquire) {
            return Err(Error::TransportClosed);
        }

        // Acquire remote receive credit (cancellation-safe: permit is
        // returned automatically if the future is dropped)
        let credit_permit = self
            .remote_credits
            .acquire()
            .await
            .map_err(|_| Error::TransportClosed)?;

        // Acquire send buffer from the pool (backpressure point)
        let mut mr = {
            let mut rx = self.send_pool_rx.lock().await;
            rx.recv().await.ok_or(Error::TransportClosed)?
        };

        // === SYNCHRONOUS SECTION (no .await, no cancellation) ===

        // Write protocol frame: header + payload
        let frame_len = protocol::write_data_frame(mr.as_mut_slice(), data);

        // Post send WR directly (avoid OpFuture Pending → Inflight transition
        // that would create a cancellation window between credit acquire and
        // WR posting)
        let qp = self.connection.shared_qp().qp().clone();
        let handle = self.connection.shared_qp().send_handle().clone();

        let reg = match handle.map().register() {
            Some(r) => r,
            None => {
                let _ = self.send_pool_tx.try_send(mr);
                // credit_permit drops → credit returned
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
            // credit_permit drops → credit returned
            return Err(e);
        }

        // WR is posted. The credit is correctly consumed — forget the permit.
        credit_permit.forget();

        // === END SYNCHRONOUS SECTION ===

        // Create inflight OpFuture and await CQE
        let send_pool_tx = self.send_pool_tx.clone();
        let op = OpFuture::new_inflight(handle, token, mr).on_cancel_reclaim(Box::new(move |mr| {
            let _ = send_pool_tx.try_send(mr);
        }));

        let (result, mr) = op.await;

        // Always return MR to pool first, even on error (prevents pool leak)
        let _ = self.send_pool_tx.try_send(mr);

        // Map flush errors to TransportClosed at the transport boundary
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

    /// The configured maximum message payload size.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Access the underlying connection.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Graceful async shutdown.
    ///
    /// Closes all internal channels, aborts the recv pump and disconnect
    /// monitor, and initiates connection shutdown.
    pub async fn close(mut self) {
        // Mark as closed
        self.closed.store(true, Ordering::Release);
        self.remote_credits.close();

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

        self.connection.initiate_shutdown();
    }
}

impl Drop for MessageTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.remote_credits.close();

        if let Some(task) = self.recv_pump_task.take() {
            task.abort();
        }
        if let Some(task) = self.disconnect_task.take() {
            task.abort();
        }
        // Connection::drop handles QP→error + driver shutdown
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Disconnect Monitor
// ═══════════════════════════════════════════════════════════════════════════

/// CM event monitor — sole consumer of the connection's event channel.
///
/// On peer disconnect or CM error, atomically closes the transport.
/// Does NOT own the `CmId` — that stays in [`Connection`] for safe
/// QP-before-CmId destruction ordering. Uses `compare_exchange` on the
/// `closed` flag to ensure idempotent shutdown.
async fn disconnect_monitor(
    handle: CmMonitorHandle,
    closed: Arc<AtomicBool>,
    credits: Arc<Semaphore>,
    driver_handles: Vec<Arc<CqDriverHandle>>,
) {
    use crate::cm::CmEventType;

    loop {
        let guard_result = handle.cm_async_fd.readable().await;
        let mut guard = match guard_result {
            Ok(g) => g,
            Err(_) => break,
        };

        match handle.event_channel.try_get_event() {
            Ok(event) => {
                let event_type = event.event_type();
                tracing::debug!(?event_type, "disconnect_monitor: CM event");
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

    if closed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        tracing::info!("disconnect_monitor: peer disconnected, closing transport");
        credits.close();
        for h in &driver_handles {
            h.flush_and_shutdown();
        }
    }

    // CmMonitorHandle drops: AsyncFd deregistered, Arc<EventChannel> decremented.
    // CmId stays in Connection — QP-before-CmId ordering maintained.
}

// ═══════════════════════════════════════════════════════════════════════════
// Recv Pump
// ═══════════════════════════════════════════════════════════════════════════

/// Background task that manages receive buffer posting, frame parsing,
/// credit handling, and completion routing.
///
/// # Responsibilities
///
/// 1. Poll pre-posted recv OpFutures for completions
/// 2. Parse incoming frame headers (DATA/CREDIT/HELLO)
/// 3. Deliver DATA frames to the user's recv channel
/// 4. Process CREDIT frames: add credits to the sender's semaphore
/// 5. Repost recv MRs from ReceivedMessage drops + send CREDIT to peer
/// 6. Repost control MRs immediately after processing
#[expect(clippy::too_many_arguments)]
async fn recv_pump(
    qp: Arc<super::qp::Qp>,
    send_handle: Arc<CqDriverHandle>,
    recv_handle: Arc<CqDriverHandle>,
    _pd: Pd, // kept alive so MRs remain valid
    initial_futures: Vec<OpFuture>,
    recv_msg_tx: mpsc::Sender<CompletedRecv>,
    mut repost_rx: mpsc::UnboundedReceiver<Mr>,
    ctrl_send_tx: mpsc::Sender<Mr>,
    mut ctrl_send_rx: mpsc::Receiver<Mr>,
    remote_credits: Arc<Semaphore>,
    closed: Arc<AtomicBool>,
) {
    let mut pending_recvs: Vec<OpFuture> = initial_futures;
    let mut pending_credits: u32 = 0;

    loop {
        if closed.load(Ordering::Acquire) {
            return;
        }

        // Try to send pending credits before blocking
        if pending_credits > 0
            && let Ok(mut ctrl_mr) = ctrl_send_rx.try_recv()
        {
            let credits_to_send = pending_credits;
            pending_credits = 0;
            let frame_len = protocol::write_credit_frame(ctrl_mr.as_mut_slice(), credits_to_send);
            let ctx = ctrl_send_tx.clone();
            let _ = post_send_and_detach(
                &qp,
                &send_handle,
                ctrl_mr,
                frame_len,
                Box::new(move |mr| {
                    let _ = ctx.try_send(mr);
                }),
            );
        }

        if pending_recvs.is_empty() {
            // All buffers are out — wait for repost, ctrl MR, or shutdown
            tokio::select! {
                biased;
                mr = repost_rx.recv() => {
                    match mr {
                        Some(mr) => {
                            pending_credits += 1;
                            match post_recv_and_track(&qp, &recv_handle, mr) {
                                Ok(future) => pending_recvs.push(future),
                                Err(_) => return,
                            }
                        }
                        None => return,
                    }
                    continue;
                }
                // Wake when a control send MR returns (so we can flush credits)
                ctrl_mr = ctrl_send_rx.recv(), if pending_credits > 0 => {
                    if let Some(mut ctrl_mr) = ctrl_mr {
                        let credits_to_send = pending_credits;
                        pending_credits = 0;
                        let frame_len = protocol::write_credit_frame(ctrl_mr.as_mut_slice(), credits_to_send);
                        let ctx = ctrl_send_tx.clone();
                        let _ = post_send_and_detach(&qp, &send_handle, ctrl_mr, frame_len, Box::new(move |mr| { let _ = ctx.try_send(mr); }));
                    }
                    continue;
                }
            }
        }

        // Wait for: a recv completion, a reposted MR, or shutdown
        tokio::select! {
            biased;
            // Poll pending recvs for the first completion
            result = poll_any_ready(&mut pending_recvs) => {
                match result {
                    PollResult::Ready(idx, Ok((completion, mr))) => {
                        pending_recvs.swap_remove(idx);
                        let byte_len = completion.byte_len() as usize;

                        // Parse frame header
                        match protocol::parse_header(mr.as_slice(), byte_len) {
                            Ok(header) => {
                                match header.frame_type {
                                    protocol::FRAME_DATA => {
                                        let payload_len = header.payload_len as usize;
                                        if recv_msg_tx.send(CompletedRecv { mr, byte_len: payload_len }).await.is_err() {
                                            return; // recv channel closed
                                        }
                                    }
                                    protocol::FRAME_CREDIT => {
                                        if let Ok(credit) = protocol::parse_credit(&mr.as_slice()[protocol::HEADER_SIZE..]) {
                                            remote_credits.add_permits(credit.credits as usize);
                                        }
                                        // Repost control buffer immediately
                                        match post_recv_and_track(&qp, &recv_handle, mr) {
                                            Ok(future) => pending_recvs.push(future),
                                            Err(_) => return,
                                        }
                                    }
                                    protocol::FRAME_HELLO => {
                                        tracing::warn!("recv_pump: unexpected HELLO during normal operation");
                                        // Repost immediately
                                        match post_recv_and_track(&qp, &recv_handle, mr) {
                                            Ok(future) => pending_recvs.push(future),
                                            Err(_) => return,
                                        }
                                    }
                                    _ => {
                                        tracing::warn!(frame_type = header.frame_type, "recv_pump: unknown frame type");
                                        match post_recv_and_track(&qp, &recv_handle, mr) {
                                            Ok(future) => pending_recvs.push(future),
                                            Err(_) => return,
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("recv_pump: protocol error: {e}");
                                // Repost the buffer and continue
                                match post_recv_and_track(&qp, &recv_handle, mr) {
                                    Ok(future) => pending_recvs.push(future),
                                    Err(_) => return,
                                }
                            }
                        }
                    }
                    PollResult::Ready(idx, Err((_err, _mr))) => {
                        pending_recvs.swap_remove(idx);
                        tracing::debug!("recv_pump: recv completion error, shutting down");
                        pending_recvs.clear();
                        return;
                    }
                }
            }
            // Drain repost channel eagerly
            mr = repost_rx.recv() => {
                match mr {
                    Some(mr) => {
                        pending_credits += 1;
                        match post_recv_and_track(&qp, &recv_handle, mr) {
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
