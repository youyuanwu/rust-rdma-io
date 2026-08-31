//! Message-oriented Send/Recv transport over RDMA.
//!
//! Provides [`MessageTransport`] with pre-registered reusable buffer pools,
//! message boundaries, bounded backpressure via application-level receive
//! credits, and cancellation-safe `send()`/`recv()` operations.
//!
//! # Progress Ownership
//!
//! Engine attachment through [`MessageTransportBuilder::connect_on`] or
//! [`MessageTransportBuilder::accept_on`] returns only [`MessageTransport`];
//! the engine driver performs message setup and protocol progress with zero
//! additional message tasks. The legacy endpoint-oriented `connect`/`accept`
//! surface remains available until the v2 surface cutover and returns a
//! `(MessageTransport, MessageTransportDriver)` pair for explicit polling.
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
//! transport.close().await?;
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
//! ## Credit Balance Invariant
//!
//! The available credit count (`remote_credits.available_permits()`) plus
//! in-flight sends must never exceed the negotiated peer receive capacity.
//! On CREDIT receipt the driver validates:
//!
//! - **Nonzero**: zero-credit frames are rejected as protocol violations.
//! - **In-flight**: returned credits ≤ `credits_in_flight` (sends that have
//!   been posted and forgotten but not yet credited back).
//! - **Capacity**: `available_permits + returned_credits <= peer_recv_capacity`
//!   (belt-and-suspenders, with overflow-safe arithmetic).
//!
//! The in-flight check is the primary invariant and is TOCTOU-safe:
//! `credits_in_flight` is incremented after `credit_permit.forget()` but
//! BEFORE `post_send_wr_raw()` in the synchronous section of `send()`.
//! This ensures the DATA cannot reach the peer before `credits_in_flight`
//! reflects it. On post failure, `send()` rolls back with `checked_sub`
//! — if a concurrent CREDIT already consumed the phantom entry, the
//! rollback harmlessly no-ops. CREDIT processing is single-threaded in
//! the driver loop.
//!
//! The peer's `data_recv_capacity` announced during HELLO is validated
//! (must be > 0 and ≤ `Semaphore::MAX_PERMITS`) before credit
//! initialization.
//!
//! A violating CREDIT triggers [`Error::ProtocolViolation`], terminates
//! the driver through normal failure/shutdown, and exposes the cause
//! through the driver result and [`MessageTransport::error()`].
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
//! The driver future internally monitors CM events. On peer disconnect,
//! the transport is cleanly closed with [`Error::TransportClosed`]. On
//! CM errors (device removal, connection fault), the error is stored as
//! [`Error::TransportFailed`] with the typed cause, waking all pending
//! send/recv/credit waiters and initiating QP/driver shutdown.

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{Mutex, Notify, Semaphore, mpsc};

use crate::async_cm::AsyncCmListener;
use crate::cm::ConnParam;
use crate::wr::{SendFlags, SendWr, Sge, WrOpcode};

use super::connection::{
    CmMonitorHandle, ConnectionBuilder, ConnectionConfig, ConnectionLifetime, ConnectionParts,
};
use super::driver::CqDriverHandle;
use super::engine::{
    CompletionMode, ConnectionReadyWork, ConnectionState, EngineShared, PreEstablishSetup,
    RdmaConnection, RdmaConnectionConfig, RdmaEngine, RdmaListener, SetupSummary,
};
use super::error::{Error, Result, TransportError};
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
    connection_config: Option<RdmaConnectionConfig>,
    #[cfg(any(test, feature = "test-hooks"))]
    hello_override: Option<TestHelloOverride>,
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
            connection_config: None,
            #[cfg(any(test, feature = "test-hooks"))]
            hello_override: None,
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

    /// Supply the base QP/CM configuration for engine-attached message transport.
    ///
    /// The configured send and receive WR maxima may exceed, but must not
    /// undershoot, the checked `send_buffers + 2 + 1` and
    /// `recv_buffers + 2` protocol requirements.
    pub fn connection_config(mut self, config: RdmaConnectionConfig) -> Self {
        self.connection_config = Some(config);
        self
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_hello_override(mut self, value: TestHelloOverride) -> Self {
        self.hello_override = Some(value);
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
        // HELLO needs one distinct send WR and reuses one control receive.
        let max_send_wr = self.send_buffer_count + ctrl_send_headroom + 1;
        let max_recv_wr = self.recv_buffer_count + ctrl_recv_count;
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

    fn derive_engine_config(&self) -> Result<EngineMessageConfig> {
        self.validate()?;
        let required_send_wr = self
            .send_buffer_count
            .checked_add(protocol::CTRL_SEND_COUNT)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| Error::InvalidConfig("message send WR derivation overflow".into()))?;
        let required_recv_wr = self
            .recv_buffer_count
            .checked_add(protocol::CTRL_RECV_COUNT)
            .ok_or_else(|| Error::InvalidConfig("message receive WR derivation overflow".into()))?;
        let mr_size = protocol::HEADER_SIZE
            .checked_add(self.buffer_size)
            .ok_or_else(|| Error::InvalidConfig("message MR size overflow".into()))?;
        u32::try_from(self.recv_buffer_count).map_err(|_| {
            Error::InvalidConfig("recv_buffers does not fit the HELLO wire field".into())
        })?;
        u32::try_from(self.buffer_size).map_err(|_| {
            Error::InvalidConfig("buffer_size does not fit the HELLO wire field".into())
        })?;
        u32::try_from(mr_size).map_err(|_| {
            Error::InvalidConfig("message MR size does not fit a receive SGE".into())
        })?;

        let connection = match self.connection_config.clone() {
            Some(config) => {
                if config.maximum_send_work_requests() < required_send_wr {
                    return Err(Error::InvalidConfig(format!(
                        "connection maximum send WRs ({}) is below the message requirement ({required_send_wr})",
                        config.maximum_send_work_requests()
                    )));
                }
                if config.maximum_receive_work_requests() < required_recv_wr {
                    return Err(Error::InvalidConfig(format!(
                        "connection maximum receive WRs ({}) is below the message requirement ({required_recv_wr})",
                        config.maximum_receive_work_requests()
                    )));
                }
                config
            }
            None => RdmaConnectionConfig::default()
                .max_send_wr(required_send_wr)
                .max_recv_wr(required_recv_wr),
        };

        Ok(EngineMessageConfig {
            connection,
            send_count: self.send_buffer_count,
            recv_count: self.recv_buffer_count,
            buffer_size: self.buffer_size,
            mr_size,
            #[cfg(any(test, feature = "test-hooks"))]
            hello_override: self.hello_override,
        })
    }

    /// Attach an outbound message transport to an RDMA engine.
    ///
    /// The returned frontend has no connection-local driver. The engine driver
    /// owns receive pre-posting, HELLO negotiation, CQ routing, and readiness.
    pub async fn connect_on(
        self,
        engine: &RdmaEngine,
        addr: SocketAddr,
    ) -> Result<MessageTransport> {
        let config = self.derive_engine_config()?;
        engine.validate_message_connection_config(&config.connection)?;
        let state = Arc::new(EngineMessageState::new(&config));
        let setup = MessagePreEstablishSetup {
            state: Arc::clone(&state),
            recv_count: config.recv_count,
            mr_size: config.mr_size,
        };
        let connection = engine
            .connect_with_setup(addr, config.connection, Box::new(setup))
            .await?;
        state.attach(&connection)?;
        Ok(MessageTransport::from_engine(
            connection,
            state,
            config.buffer_size,
        ))
    }

    /// Attach an inbound message transport to an engine-owned listener.
    ///
    /// This registers one message waiter in the listener's existing ordered
    /// accept queue and returns no listener- or message-specific driver.
    pub async fn accept_on(self, listener: &RdmaListener) -> Result<MessageTransport> {
        let config = self.derive_engine_config()?;
        listener.validate_message_connection_config(&config.connection)?;
        let state = Arc::new(EngineMessageState::new(&config));
        let setup = MessagePreEstablishSetup {
            state: Arc::clone(&state),
            recv_count: config.recv_count,
            mr_size: config.mr_size,
        };
        let connection = listener
            .accept_with_setup(config.connection, Box::new(setup))
            .await?;
        state.attach(&connection)?;
        Ok(MessageTransport::from_engine(
            connection,
            state,
            config.buffer_size,
        ))
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

struct EngineMessageConfig {
    connection: RdmaConnectionConfig,
    send_count: usize,
    recv_count: usize,
    buffer_size: usize,
    mr_size: usize,
    #[cfg(any(test, feature = "test-hooks"))]
    hello_override: Option<TestHelloOverride>,
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum TestHelloOverride {
    BadMagic,
    BadVersion,
    WrongFrameType,
    ZeroReceiveCredits,
    SmallerMaximumMessage,
}

struct MessagePreEstablishSetup {
    state: Arc<EngineMessageState>,
    recv_count: usize,
    mr_size: usize,
}

impl PreEstablishSetup for MessagePreEstablishSetup {
    fn run(self: Box<Self>, connection: &RdmaConnection) -> Result<SetupSummary> {
        let total = self
            .recv_count
            .checked_add(protocol::CTRL_RECV_COUNT)
            .ok_or_else(|| Error::InvalidConfig("message receive batch overflow".into()))?;
        let mut entries = Vec::with_capacity(total);
        for _ in 0..total {
            let mr = connection.register_memory(self.mr_size, AccessIntent::LocalOnly)?;
            let state = Arc::downgrade(&self.state);
            entries.push((
                mr,
                Box::new(move |result, mr| {
                    if let Some(state) = state.upgrade() {
                        state.enqueue_event(EngineMessageEvent::Receive { result, mr });
                    }
                }) as _,
            ));
        }
        let posted = connection.post_detached_recv_batch(entries)?;
        Ok(SetupSummary { posted_wrs: posted })
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

/// Pure credit-return validation logic (no side effects).
///
/// Returns `Ok(())` if the CREDIT is valid, or `Err(ProtocolViolation)` if:
/// - `credits == 0` (zero-credit frame)
/// - `credits > in_flight` (more credits returned than sends in flight)
/// - `available + credits > capacity` (would exceed negotiated capacity,
///   checked with `saturating_add` to prevent overflow on 32-bit targets)
///
/// Extracted as a free function for testability without RDMA hardware.
fn check_credit_return(
    credits: u32,
    in_flight: usize,
    available: usize,
    capacity: usize,
) -> Result<()> {
    if credits == 0 {
        return Err(Error::ProtocolViolation(
            "CREDIT frame with zero credits".into(),
        ));
    }
    let credits_usize = credits as usize;
    // Primary invariant: can only return credits for in-flight sends.
    // Immune to acquire→forget TOCTOU: only forgotten permits are counted.
    if credits_usize > in_flight {
        return Err(Error::ProtocolViolation(format!(
            "CREDIT exceeds in-flight sends: returned={credits_usize} > in_flight={in_flight}"
        )));
    }
    // Belt-and-suspenders: check against capacity with overflow protection.
    if available.saturating_add(credits_usize) > capacity {
        return Err(Error::ProtocolViolation(format!(
            "CREDIT would exceed negotiated capacity: \
             available={available} + returned={credits_usize} > capacity={capacity}"
        )));
    }
    Ok(())
}

/// Shared lifecycle state between [`MessageTransport`] and [`MessageTransportDriver`].
pub(crate) struct TransportSharedState {
    /// Lifecycle state machine.
    pub(crate) state: AtomicU8,
    /// Wakes `ready()`, `send()`, `recv()`, `close()` on state changes.
    pub(crate) state_notify: Notify,
    /// Remote receive credits (initialized empty, filled by driver after HELLO).
    pub(crate) remote_credits: Semaphore,
    /// Negotiated peer receive capacity from HELLO handshake.
    ///
    /// Set once during HELLO to the peer's `data_recv_capacity`. The credit
    /// balance invariant enforced by [`Self::validate_and_add_credits`] is:
    ///
    /// `remote_credits.available_permits() + credits_in_flight <= peer_recv_capacity`
    ///
    /// This is immutable once set (only transitions from 0 → peer value).
    pub(crate) peer_recv_capacity: AtomicUsize,
    /// Number of send permits consumed (forgotten) but not yet returned
    /// via CREDIT frames. Incremented in `send()` after `permit.forget()`,
    /// decremented in `validate_and_add_credits()` after validation passes.
    ///
    /// This field is the primary invariant for CREDIT validation: a CREDIT
    /// returning `k` credits is valid only if `k <= credits_in_flight`,
    /// because a credit can only be generated after the corresponding DATA
    /// was received by the peer. This is immune to the acquire→forget
    /// TOCTOU: temporarily-acquired (but not-yet-forgotten) permits do NOT
    /// increment this counter, so a cancelled/failed send cannot inflate
    /// the allowance for a concurrent CREDIT.
    pub(crate) credits_in_flight: AtomicUsize,
    /// Whether the frontend is still alive.
    pub(crate) frontend_alive: AtomicBool,
    /// Connection lifetime owner — MUST drop FIRST to destroy QP before MRs are freed.
    /// Holds SharedQp, Pd, CmId, EventChannel in safe drop order. All QP access
    /// is borrowed through this lease.
    pub(crate) conn_lifetime: Arc<ConnectionLifetime>,
    /// Driver handle refs for shutdown — dropping these frees quarantined MRs.
    /// MUST drop AFTER `conn_lifetime`.
    pub(crate) driver_handles: Vec<Arc<CqDriverHandle>>,
    /// Terminal error snapshot (stored once, readable from frontend).
    pub(crate) error: std::sync::Mutex<Option<TransportError>>,
}

impl TransportSharedState {
    fn new(
        conn_lifetime: Arc<ConnectionLifetime>,
        driver_handles: Vec<Arc<CqDriverHandle>>,
    ) -> Self {
        Self {
            state: AtomicU8::new(STATE_CREATED),
            state_notify: Notify::new(),
            remote_credits: Semaphore::new(0),
            peer_recv_capacity: AtomicUsize::new(0),
            credits_in_flight: AtomicUsize::new(0),
            frontend_alive: AtomicBool::new(true),
            conn_lifetime,
            driver_handles,
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

    /// Initialize credits from the negotiated peer capacity after HELLO.
    ///
    /// Sets `peer_recv_capacity` (immutable once set) and adds the initial
    /// permits to the semaphore. Must be called exactly once, before
    /// transitioning to READY.
    fn init_credits(&self, capacity: usize) {
        self.peer_recv_capacity.store(capacity, Ordering::Release);
        self.remote_credits.add_permits(capacity);
    }

    /// Record that a send permit was consumed (forgotten). Called from
    /// `send()` immediately after `credit_permit.forget()`, BEFORE
    /// `post_send_wr_raw()`.
    ///
    /// This ensures that when the DATA reaches the wire and the peer
    /// returns a CREDIT, `credits_in_flight` already reflects the send.
    /// On post failure, `send()` rolls back via `checked_sub` (see the
    /// rollback comment in `send()`'s synchronous section).
    fn record_send(&self) {
        self.credits_in_flight.fetch_add(1, Ordering::Release);
    }

    /// Validate and add credits returned by a peer CREDIT frame.
    ///
    /// Enforces two independent invariants:
    ///
    /// 1. **In-flight check** (primary, TOCTOU-safe):
    ///    `credits <= credits_in_flight`. A CREDIT can only return credits
    ///    for DATA WRs that have been posted (permits forgotten). Concurrent
    ///    `send()` only increases `credits_in_flight` (via `record_send()`
    ///    before `post_send_wr_raw()`), which makes this check conservative.
    ///    On post failure, `send()` rolls back with `checked_sub` — if a
    ///    concurrent CREDIT consumed the phantom entry, the rollback
    ///    harmlessly no-ops (the credit accounting is already correct).
    ///
    /// 2. **Capacity check** (belt-and-suspenders):
    ///    `available_permits + credits <= peer_recv_capacity`. Guards against
    ///    any bug in the in-flight tracking. Uses checked arithmetic to
    ///    prevent overflow on 32-bit targets.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ProtocolViolation`] if:
    /// - `credits == 0` — a zero-credit frame is a protocol violation
    /// - Credits exceed in-flight sends or would exceed negotiated capacity
    fn validate_and_add_credits(&self, credits: u32) -> Result<()> {
        let k = credits as usize;
        check_credit_return(
            credits,
            self.credits_in_flight.load(Ordering::Acquire),
            self.remote_credits.available_permits(),
            self.peer_recv_capacity.load(Ordering::Acquire),
        )?;
        // Decrement in_flight atomically with checked_sub. If a concurrent
        // send() rollback already consumed the entry (race between
        // post_send_wr_raw failure and CREDIT processing), the checked_sub
        // fails and we reject the CREDIT — the capacity check would also
        // catch this since the rollback already returned the permit.
        self.credits_in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(k))
            .map_err(|v| {
                Error::ProtocolViolation(format!(
                    "CREDIT exceeds in-flight sends (atomic): returned={k} > in_flight={v}"
                ))
            })?;
        self.remote_credits.add_permits(k);
        Ok(())
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
        let _ = self.conn_lifetime.shared_qp().shutdown();
        for h in &self.driver_handles {
            h.close_and_shutdown();
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

struct EngineMessageLink {
    shared: Weak<EngineShared>,
    connection: Weak<ConnectionState>,
}

struct EngineHandshake {
    hello_send_posted: bool,
    hello_send_complete: bool,
    hello_receive_complete: bool,
}

enum EngineMessageEvent {
    Start,
    Send {
        result: Result<super::op::Completion>,
        mr: Option<Mr>,
    },
    Receive {
        result: Result<super::op::Completion>,
        mr: Option<Mr>,
    },
}

struct EngineMessageState {
    state: AtomicU8,
    state_notify: Notify,
    error: StdMutex<Option<Error>>,
    remote_credits: Semaphore,
    peer_recv_capacity: AtomicUsize,
    local_recv_capacity: usize,
    _local_send_capacity: usize,
    buffer_size: usize,
    handshake: StdMutex<EngineHandshake>,
    events: StdMutex<VecDeque<EngineMessageEvent>>,
    steady_inbox: StdMutex<VecDeque<(Result<super::op::Completion>, Option<Mr>)>>,
    link: OnceLock<EngineMessageLink>,
    self_weak: OnceLock<Weak<EngineMessageState>>,
    #[cfg(any(test, feature = "test-hooks"))]
    hello_override: Option<TestHelloOverride>,
}

impl EngineMessageState {
    fn new(config: &EngineMessageConfig) -> Self {
        Self {
            state: AtomicU8::new(STATE_CREATED),
            state_notify: Notify::new(),
            error: StdMutex::new(None),
            remote_credits: Semaphore::new(0),
            peer_recv_capacity: AtomicUsize::new(0),
            local_recv_capacity: config.recv_count,
            _local_send_capacity: config.send_count,
            buffer_size: config.buffer_size,
            handshake: StdMutex::new(EngineHandshake {
                hello_send_posted: false,
                hello_send_complete: false,
                hello_receive_complete: false,
            }),
            events: StdMutex::new(VecDeque::new()),
            steady_inbox: StdMutex::new(VecDeque::new()),
            link: OnceLock::new(),
            self_weak: OnceLock::new(),
            #[cfg(any(test, feature = "test-hooks"))]
            hello_override: config.hello_override,
        }
    }

    fn attach(self: &Arc<Self>, connection: &RdmaConnection) -> Result<()> {
        connection.attach_ready_work(Arc::clone(self) as Arc<dyn ConnectionReadyWork>)?;
        self.self_weak
            .set(Arc::downgrade(self))
            .map_err(|_| Error::InvalidConfig("message state attached more than once".into()))?;
        self.link
            .set(EngineMessageLink {
                shared: Arc::downgrade(&connection.shared),
                connection: Arc::downgrade(&connection.state),
            })
            .map_err(|_| Error::InvalidConfig("message state attached more than once".into()))?;
        self.enqueue_event(EngineMessageEvent::Start);
        connection.publish_ready_work();
        Ok(())
    }

    fn weak_self(&self) -> Weak<Self> {
        self.self_weak.get().cloned().unwrap_or_default()
    }

    fn enqueue_event(&self, event: EngineMessageEvent) {
        lock_std(&self.events).push_back(event);
        self.publish();
    }

    fn publish(&self) {
        let Some(link) = self.link.get() else {
            return;
        };
        let (Some(shared), Some(connection)) = (link.shared.upgrade(), link.connection.upgrade())
        else {
            return;
        };
        shared.publish_connection_ready(&connection);
    }

    fn fail(&self, error: Error, close_connection: bool) {
        let state = self.state.load(Ordering::Acquire);
        if matches!(state, STATE_STOPPED | STATE_FAILED) {
            return;
        }
        {
            let mut stored = lock_std(&self.error);
            if stored.is_none() {
                *stored = Some(error);
            }
        }
        self.state.store(STATE_FAILED, Ordering::Release);
        self.remote_credits.close();
        self.state_notify.notify_waiters();
        if close_connection
            && let Some(link) = self.link.get()
            && let (Some(shared), Some(connection)) =
                (link.shared.upgrade(), link.connection.upgrade())
        {
            shared.begin_connection_close(&connection);
        }
    }

    fn terminal_error(&self) -> Error {
        lock_std(&self.error)
            .clone()
            .unwrap_or(Error::TransportClosed)
    }

    async fn ready(&self) -> Result<()> {
        loop {
            let notified = self.state_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.state.load(Ordering::Acquire) {
                STATE_READY => return Ok(()),
                STATE_FAILED => return Err(self.terminal_error()),
                STATE_CLOSING | STATE_STOPPED => return Err(Error::TransportClosed),
                _ => notified.await,
            }
        }
    }

    fn begin_close(&self) {
        let _ = self.state.compare_exchange(
            STATE_CREATED,
            STATE_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let _ = self.state.compare_exchange(
            STATE_READY,
            STATE_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.remote_credits.close();
        self.state_notify.notify_waiters();
    }

    fn finish_close(&self, result: &Result<()>) {
        match result {
            Ok(()) => {
                if self.state.load(Ordering::Acquire) != STATE_FAILED {
                    self.state.store(STATE_STOPPED, Ordering::Release);
                }
            }
            Err(error) => self.fail(error.clone(), false),
        }
        self.state_notify.notify_waiters();
    }

    fn process_event(&self, event: EngineMessageEvent) {
        match event {
            EngineMessageEvent::Start => self.start_hello_send(),
            EngineMessageEvent::Send { result, mr } => {
                drop(mr);
                match result {
                    Ok(_) => {
                        lock_std(&self.handshake).hello_send_complete = true;
                        self.try_mark_ready();
                    }
                    Err(error) => self.fail(error, true),
                }
            }
            EngineMessageEvent::Receive { result, mr } => {
                if self.state.load(Ordering::Acquire) == STATE_READY {
                    lock_std(&self.steady_inbox).push_back((result, mr));
                    return;
                }
                if self.state.load(Ordering::Acquire) != STATE_CREATED {
                    drop(mr);
                    return;
                }
                self.process_hello_receive(result, mr);
            }
        }
    }

    fn connection(&self) -> Result<RdmaConnection> {
        let link = self
            .link
            .get()
            .ok_or_else(|| Error::InvalidConfig("message state has no engine connection".into()))?;
        let shared = link.shared.upgrade().ok_or(Error::TransportClosed)?;
        let connection = link.connection.upgrade().ok_or(Error::TransportClosed)?;
        Ok(RdmaConnection::from_state(shared, connection))
    }

    fn start_hello_send(&self) {
        if self.state.load(Ordering::Acquire) != STATE_CREATED {
            return;
        }
        {
            let mut handshake = lock_std(&self.handshake);
            if handshake.hello_send_posted {
                return;
            }
            handshake.hello_send_posted = true;
        }
        let connection = match self.connection() {
            Ok(connection) => connection,
            Err(error) => {
                self.fail(error, false);
                return;
            }
        };
        let mut mr =
            match connection.register_memory(protocol::HELLO_FRAME_SIZE, AccessIntent::LocalOnly) {
                Ok(mr) => mr,
                Err(error) => {
                    self.fail(error, true);
                    return;
                }
            };
        #[allow(unused_mut)]
        let mut advertised_recv = self.local_recv_capacity as u32;
        #[allow(unused_mut)]
        let mut advertised_size = self.buffer_size as u32;
        #[cfg(any(test, feature = "test-hooks"))]
        {
            match self.hello_override {
                Some(TestHelloOverride::ZeroReceiveCredits) => advertised_recv = 0,
                Some(TestHelloOverride::SmallerMaximumMessage) => {
                    advertised_size = advertised_size.saturating_sub(1);
                }
                _ => {}
            }
        }
        let len = protocol::write_hello_frame(mr.as_mut_slice(), advertised_recv, advertised_size);
        #[cfg(any(test, feature = "test-hooks"))]
        {
            match self.hello_override {
                Some(TestHelloOverride::BadMagic) => mr.as_mut_slice()[0] ^= 0xff,
                Some(TestHelloOverride::BadVersion) => {
                    mr.as_mut_slice()[4] = protocol::PROTO_VERSION.wrapping_add(1);
                }
                Some(TestHelloOverride::WrongFrameType) => {
                    mr.as_mut_slice()[5] = protocol::FRAME_DATA;
                }
                _ => {}
            }
        }
        let state = self.weak_self();
        if let Err(error) = connection.post_detached_send(
            mr,
            len,
            Box::new(move |result, mr| {
                if let Some(state) = state.upgrade() {
                    state.enqueue_event(EngineMessageEvent::Send { result, mr });
                }
            }),
        ) {
            self.fail(error, true);
        }
    }

    fn process_hello_receive(&self, result: Result<super::op::Completion>, mr: Option<Mr>) {
        let completion = match result {
            Ok(completion) => completion,
            Err(error) => {
                drop(mr);
                self.fail(error, true);
                return;
            }
        };
        let Some(mr) = mr else {
            self.fail(Error::DriverShutdown, true);
            return;
        };
        let received_len = completion.byte_len() as usize;
        if received_len > mr.len() {
            self.fail(
                Error::ProtocolViolation(format!(
                    "HELLO receive length {received_len} exceeds MR length {}",
                    mr.len()
                )),
                true,
            );
            return;
        }
        let header = match protocol::parse_header(mr.as_slice(), received_len) {
            Ok(header) => header,
            Err(error) => {
                self.fail(error, true);
                return;
            }
        };
        if header.frame_type != protocol::FRAME_HELLO {
            self.fail(
                Error::ProtocolViolation(format!(
                    "expected HELLO, got frame_type={}",
                    header.frame_type
                )),
                true,
            );
            return;
        }
        let payload_end = match protocol::HEADER_SIZE.checked_add(header.payload_len as usize) {
            Some(end) => end,
            None => {
                self.fail(
                    Error::ProtocolViolation("HELLO payload length overflow".into()),
                    true,
                );
                return;
            }
        };
        let hello = match protocol::parse_hello(&mr.as_slice()[protocol::HEADER_SIZE..payload_end])
        {
            Ok(hello) => hello,
            Err(error) => {
                self.fail(error, true);
                return;
            }
        };
        let peer_capacity = match validate_peer_hello(hello, self.buffer_size) {
            Ok(capacity) => capacity,
            Err(error) => {
                self.fail(error, true);
                return;
            }
        };
        if lock_std(&self.handshake).hello_receive_complete {
            self.fail(
                Error::ProtocolViolation("duplicate HELLO during negotiation".into()),
                true,
            );
            return;
        }
        let connection = match self.connection() {
            Ok(connection) => connection,
            Err(error) => {
                self.fail(error, false);
                return;
            }
        };
        let state = self.weak_self();
        if let Err(error) = connection.post_detached_recv_batch(vec![(
            mr,
            Box::new(move |result, mr| {
                if let Some(state) = state.upgrade() {
                    state.enqueue_event(EngineMessageEvent::Receive { result, mr });
                }
            }),
        )]) {
            self.fail(error, true);
            return;
        }
        self.peer_recv_capacity
            .store(peer_capacity, Ordering::Release);
        self.remote_credits.add_permits(peer_capacity);
        lock_std(&self.handshake).hello_receive_complete = true;
        self.try_mark_ready();
    }

    fn try_mark_ready(&self) {
        let handshake = lock_std(&self.handshake);
        if !handshake.hello_send_complete || !handshake.hello_receive_complete {
            return;
        }
        drop(handshake);
        if self
            .state
            .compare_exchange(
                STATE_CREATED,
                STATE_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.state_notify.notify_waiters();
        }
    }
}

impl ConnectionReadyWork for EngineMessageState {
    fn process(&self, budget: usize) -> usize {
        let mut processed = 0;
        while processed < budget {
            let Some(event) = lock_std(&self.events).pop_front() else {
                break;
            };
            self.process_event(event);
            processed += 1;
        }
        processed
    }

    fn has_work(&self) -> bool {
        !lock_std(&self.events).is_empty()
    }

    fn deadline_expired(&self) {
        if self.state.load(Ordering::Acquire) == STATE_CREATED {
            self.fail(
                Error::ProtocolViolation("HELLO handshake timeout".into()),
                true,
            );
        }
    }

    fn disconnected(&self) {
        self.fail(Error::TransportClosed, true);
    }

    fn terminalize(&self, error: Error) {
        self.fail(error, false);
    }
}

fn lock_std<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn validate_peer_hello(hello: protocol::HelloPayload, local_buffer_size: usize) -> Result<usize> {
    let peer_capacity = hello.data_recv_capacity as usize;
    if peer_capacity == 0 {
        return Err(Error::ProtocolViolation(
            "peer data_recv_capacity is 0".into(),
        ));
    }
    if peer_capacity > Semaphore::MAX_PERMITS {
        return Err(Error::ProtocolViolation(format!(
            "peer data_recv_capacity {peer_capacity} exceeds maximum ({})",
            Semaphore::MAX_PERMITS
        )));
    }
    let peer_max = hello.max_message_size as usize;
    if peer_max < local_buffer_size {
        return Err(Error::ProtocolViolation(format!(
            "peer max_message_size {peer_max} < local buffer_size {local_buffer_size}"
        )));
    }
    Ok(peer_capacity)
}

/// Message-oriented RDMA transport (frontend handle).
///
/// Provides async `send()` and `recv()` with pre-registered reusable
/// buffer pools, message boundaries, credit-based flow control, and
/// deterministic lifecycle management.
///
/// Engine-attached transports use the owning engine driver and return no
/// message driver. The retained endpoint-oriented construction path still
/// pairs this frontend with [`MessageTransportDriver`].
///
/// # Task Count
///
/// Engine attachment adds zero tasks beyond the engine driver. The retained
/// endpoint-oriented path uses one user-spawned driver task per endpoint.
/// `send()` and `recv()` run in the caller's task context.
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
    backend: MessageTransportBackend,
}

struct LegacyMessageFrontend {
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

struct EngineMessageFrontend {
    connection: RdmaConnection,
    state: Arc<EngineMessageState>,
}

enum MessageTransportBackend {
    Legacy(LegacyMessageFrontend),
    Engine(EngineMessageFrontend),
}

impl MessageTransport {
    fn from_engine(
        connection: RdmaConnection,
        state: Arc<EngineMessageState>,
        buffer_size: usize,
    ) -> Self {
        Self {
            buffer_size,
            backend: MessageTransportBackend::Engine(EngineMessageFrontend { connection, state }),
        }
    }

    fn legacy(&self) -> &LegacyMessageFrontend {
        match &self.backend {
            MessageTransportBackend::Legacy(legacy) => legacy,
            MessageTransportBackend::Engine(_) => {
                unreachable!("engine-attached message transport uses its engine backend")
            }
        }
    }

    /// Build a transport pair from established connection parts.
    async fn from_parts(
        mut parts: ConnectionParts,
        send_count: usize,
        recv_count: usize,
        buffer_size: usize,
        pre_posted_recvs: Vec<OpFuture>,
    ) -> Result<(Self, MessageTransportDriver)> {
        let data_mr_size = protocol::data_mr_size(buffer_size);

        // Build the ConnectionLifetime — the single shared owner of all
        // RDMA resources. Drop order: SharedQp → Pd → CmId → EventChannel.
        let conn_lifetime = Arc::new(ConnectionLifetime::new(
            parts.shared_qp,
            parts.completion_channels,
            parts.pd,
            parts.cm_id,
            parts.event_channel,
        ));

        let driver_handles = parts.driver_handles;

        let shared_state = Arc::new(TransportSharedState::new(
            Arc::clone(&conn_lifetime),
            driver_handles.clone(),
        ));

        // === Allocate send buffers ===
        let pd = conn_lifetime.shared_qp().pd().clone();
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

        // NOTE: Do NOT clone send_pool_tx or repost_tx into the driver.
        // The driver must not hold these senders — when the frontend drops,
        // the channel closures signal the driver to shut down.
        let driver_future: Pin<Box<dyn Future<Output = Result<()>> + Send>> = Box::pin(driver_run(
            Arc::clone(&shared_state),
            Arc::clone(&conn_lifetime),
            driver_send_handle,
            driver_recv_handle,
            driver_handles.clone(),
            parts.driver_futures,
            cm_monitor_handle,
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
            backend: MessageTransportBackend::Legacy(LegacyMessageFrontend {
                send_pool_tx,
                send_pool_rx: Arc::new(Mutex::new(send_pool_rx)),
                recv_msg_rx: Arc::new(Mutex::new(recv_msg_rx)),
                repost_tx,
                state: Arc::clone(&shared_state),
            }),
        };

        let driver = MessageTransportDriver {
            inner: Some(driver_future),
            state: shared_state,
        };

        Ok((transport, driver))
    }

    /// Wait for the transport to become ready (HELLO handshake complete).
    pub async fn ready(&self) -> Result<()> {
        if let MessageTransportBackend::Engine(engine) = &self.backend {
            return engine.state.ready().await;
        }
        let state = &self.legacy().state;
        loop {
            let notified = state.state_notify.notified();
            let s = state.state.load(Ordering::Acquire);
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
        if matches!(self.backend, MessageTransportBackend::Engine(_)) {
            return self.ready().await;
        }
        let s = self.legacy().state.state.load(Ordering::Acquire);
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
        match &self.backend {
            MessageTransportBackend::Legacy(legacy) => legacy.state.error.lock().unwrap().clone(),
            MessageTransportBackend::Engine(engine) => lock_std(&engine.state.error)
                .as_ref()
                .map(TransportError::from_error),
        }
    }

    /// Return the stored terminal error (if any) or fall back to
    /// [`Error::TransportClosed`].
    ///
    /// Used by `ready()`, `send()`, `recv()` to surface actionable
    /// error information rather than an opaque `TransportClosed`.
    fn terminal_error(&self) -> Error {
        match &self.backend {
            MessageTransportBackend::Legacy(legacy) => {
                if let Some(te) = legacy.state.error.lock().unwrap().clone() {
                    Error::TransportFailed(te)
                } else {
                    Error::TransportClosed
                }
            }
            MessageTransportBackend::Engine(engine) => engine.state.terminal_error(),
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
        if matches!(self.backend, MessageTransportBackend::Engine(_)) {
            self.await_ready().await?;
            return Err(Error::InvalidConfig(
                "engine-attached DATA progress is introduced in Phase 8".into(),
            ));
        }
        let legacy = self.legacy();

        // Await readiness (HELLO must be complete)
        self.await_ready().await?;

        // Acquire remote receive credit
        let credit_permit = self
            .legacy()
            .state
            .remote_credits
            .acquire()
            .await
            .map_err(|_| self.terminal_error())?;

        // Acquire send buffer from the pool.
        // Race against terminal state to avoid hanging forever if the
        // driver dies while all send MRs are quarantined (FR-011).
        //
        // Uses register-check-recheck protocol: create `notified()` FIRST
        // (snapshots the notify_waiters counter), THEN check terminal state.
        // If `notify_waiters()` fires between the check and the await,
        // the already-registered `Notified` wakes immediately. Reversing
        // this order opens a lost-wakeup window where `mark_driver_dead()`
        // signals between the terminal check and the `notified()` snapshot.
        let mut mr = {
            let mut rx = legacy.send_pool_rx.lock().await;
            loop {
                let notified = legacy.state.state_notify.notified();
                if legacy.state.is_terminal() {
                    return Err(self.terminal_error());
                }
                tokio::select! {
                    biased;
                    res = rx.recv() => break res.ok_or_else(|| self.terminal_error())?,
                    () = notified => continue,
                }
            }
        };

        // === SYNCHRONOUS SECTION ===
        let frame_len = protocol::write_data_frame(mr.as_mut_slice(), data);
        let sqp = legacy.state.conn_lifetime.shared_qp();
        let qp = sqp.qp().clone();
        let handle = sqp.send_handle().clone();

        let reg = match handle.map().register() {
            Some(r) => r,
            None => {
                let _ = legacy.send_pool_tx.try_send(mr);
                return Err(if handle.map().is_closed() {
                    self.terminal_error()
                } else {
                    Error::CapacityExhausted
                });
            }
        };
        let token = reg.token;

        let addr = mr.addr();
        let sge = Sge::new(addr, frame_len as u32, mr.lkey());
        let mut wr = SendWr::new(token, WrOpcode::Send)
            .sg(sge)
            .flags(SendFlags::SIGNALED);

        // Commit the credit consumption BEFORE posting the WR to the
        // hardware. Once `post_send_wr_raw` succeeds, the DATA is on the
        // wire and a multi-threaded runtime may deliver the peer's CREDIT
        // response before any subsequent instruction executes. If
        // `record_send` ran after the post, the driver could see
        // `credits_in_flight == 0` and spuriously reject a valid CREDIT.
        //
        // On post failure, we roll back with `checked_sub` to handle the
        // narrow race where a concurrent CREDIT already consumed the
        // phantom in_flight entry. If the decrement succeeds, we also
        // return the permit; if it fails (CREDIT already consumed it),
        // the available permits are already correct.
        credit_permit.forget();
        legacy.state.record_send();

        if let Err(e) = qp.post_send_wr_raw(&mut wr) {
            // Roll back credit consumption — DATA never posted.
            // Use checked_sub: if a concurrent CREDIT already consumed
            // our phantom in_flight, the counter is 0 and we must not
            // underflow. In that case the driver already added the
            // permit via validate_and_add_credits.
            if self
                .legacy()
                .state
                .credits_in_flight
                .fetch_update(Ordering::Release, Ordering::Acquire, |v| v.checked_sub(1))
                .is_ok()
            {
                legacy.state.remote_credits.add_permits(1);
            }
            handle.map().release(token);
            let _ = legacy.send_pool_tx.try_send(mr);
            return Err(e);
        }
        handle.notify_work();
        // === END SYNCHRONOUS SECTION ===

        let send_pool_tx = legacy.send_pool_tx.clone();
        let op = OpFuture::new_inflight(handle, token, mr).on_cancel_reclaim(Box::new(move |mr| {
            let _ = send_pool_tx.try_send(mr);
        }));

        let (result, mr) = op.await;
        // MR is Some on real CQE, None if quarantined (driver shutdown).
        // Only return to pool if we actually got the MR back.
        if let Some(mr) = mr {
            let _ = legacy.send_pool_tx.try_send(mr);
        }

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
    /// Already-delivered messages remain drainable even after the transport
    /// enters a terminal state. The readiness gate only blocks when no
    /// messages are buffered and the HELLO handshake has not yet completed.
    ///
    /// # Errors
    ///
    /// - [`Error::TransportClosed`] if cleanly shut down with no buffered messages
    /// - [`Error::TransportFailed`] if the driver failed with no buffered messages
    pub async fn recv(&self) -> Result<ReceivedMessage> {
        if matches!(self.backend, MessageTransportBackend::Engine(_)) {
            self.await_ready().await?;
            return Err(Error::InvalidConfig(
                "engine-attached DATA progress is introduced in Phase 8".into(),
            ));
        }
        let legacy = self.legacy();
        let mut rx = legacy.recv_msg_rx.lock().await;

        // Drain already-delivered messages regardless of lifecycle state.
        if let Ok(completed) = rx.try_recv() {
            return Ok(ReceivedMessage {
                mr: Some(completed.mr),
                byte_len: completed.byte_len,
                repost_tx: legacy.repost_tx.clone(),
            });
        }

        // Nothing buffered: gate on readiness only while handshake is pending.
        drop(rx); // release lock during ready() wait
        let s = legacy.state.state.load(Ordering::Acquire);
        if s == STATE_CREATED {
            self.ready().await?;
        } else if s == STATE_FAILED {
            return Err(self.terminal_error());
        } else if s >= STATE_CLOSING {
            return Err(Error::TransportClosed);
        }

        // Re-acquire lock and wait for next message or channel closure.
        let mut rx = legacy.recv_msg_rx.lock().await;
        rx.recv()
            .await
            .map(|completed| ReceivedMessage {
                mr: Some(completed.mr),
                byte_len: completed.byte_len,
                repost_tx: legacy.repost_tx.clone(),
            })
            .ok_or_else(|| self.terminal_error())
    }

    /// The configured maximum message payload size.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Graceful async shutdown.
    ///
    /// Engine-attached transports close their engine-owned connection and
    /// return its contextual result. Endpoint-oriented transports signal
    /// their explicit driver and wait for its terminal state; the caller still
    /// owns and awaits that driver's `JoinHandle`.
    ///
    /// Returns immediately if the driver has already stopped (including
    /// if the driver was never spawned and was dropped). If the driver
    /// has been constructed but neither spawned nor dropped, `close()`
    /// waits indefinitely; drop the driver to force a terminal state.
    pub async fn close(&self) -> Result<()> {
        if let MessageTransportBackend::Engine(engine) = &self.backend {
            engine.state.begin_close();
            let prior_error = lock_std(&engine.state.error).clone();
            let result = engine.connection.close().await;
            engine.state.finish_close(&result);
            return match result {
                Err(error) => Err(error),
                Ok(()) => prior_error.map_or(Ok(()), Err),
            };
        }
        let state = &self.legacy().state;
        // Try to transition to Closing atomically
        let _ = state.state.compare_exchange(
            STATE_CREATED,
            STATE_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let _ = state.state.compare_exchange(
            STATE_READY,
            STATE_CLOSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        state.remote_credits.close();
        state.state_notify.notify_waiters();

        // Wait for terminal state (or return immediately if already terminal)
        loop {
            let notified = state.state_notify.notified();
            if state.is_terminal() {
                return Ok(());
            }
            notified.await;
        }
    }
}

impl Drop for MessageTransport {
    fn drop(&mut self) {
        match &self.backend {
            MessageTransportBackend::Legacy(legacy) => {
                legacy.state.frontend_alive.store(false, Ordering::Release);
                legacy.state.remote_credits.close();
                legacy.state.state_notify.notify_waiters();
            }
            MessageTransportBackend::Engine(engine) => engine.state.begin_close(),
        }
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
///
/// # Connection Lifetime
///
/// The driver retains `Arc<ConnectionLifetime>` (`conn_lifetime`) for
/// the full duration. This guarantees that CmId/EventChannel remain alive
/// while the driver uses QP/CQ resources. When the driver future drops,
/// the Arc refcount decreases; if the frontend also dropped, the
/// ConnectionLifetime destructs in safe order (QP before CmId).
#[expect(clippy::too_many_arguments)]
async fn driver_run(
    state: Arc<TransportSharedState>,
    conn_lifetime: Arc<ConnectionLifetime>,
    send_handle: Arc<CqDriverHandle>,
    recv_handle: Arc<CqDriverHandle>,
    driver_handles: Vec<Arc<CqDriverHandle>>,
    cq_driver_futures: Vec<Pin<Box<dyn Future<Output = super::error::Result<()>> + Send>>>,
    cm_monitor_handle: Option<CmMonitorHandle>,
    initial_recvs: Vec<OpFuture>,
    recv_count: usize,
    buffer_size: usize,
    recv_msg_tx: mpsc::Sender<CompletedRecv>,
    mut repost_rx: mpsc::UnboundedReceiver<Mr>,
    ctrl_send_tx: mpsc::Sender<Mr>,
    mut ctrl_send_rx: mpsc::Receiver<Mr>,
) -> Result<()> {
    use crate::cm::CmEventType;

    // Borrow shared QP through the connection lifetime lease.
    // This is a reference, not an Arc clone — the Arc<ConnectionLifetime>
    // parameter keeps everything alive.
    let shared_qp = conn_lifetime.shared_qp();

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
    let mut hello_send = Box::pin(shared_qp.send(hello_mr, Some((0, hello_len))));

    // Phase A loop: drive CQ futures while waiting for HELLO send
    let mut hello_sent = false;
    let mut peer_capacity: Option<usize> = None;

    /// Named constant for HELLO handshake timeout.
    const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
    /// Named constant for CQ drain timeout.
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

    let hello_timeout = tokio::time::sleep(HELLO_TIMEOUT);
    tokio::pin!(hello_timeout);

    // Unified terminal error tracker — shared across Phase A and Phase B.
    let mut terminal_error: Option<Error> = None;

    // Set up disconnect monitor BEFORE Phase A so both phases can use it.
    let disconnect_future = async {
        if let Some(handle) = cm_monitor_handle {
            loop {
                let guard_result = handle.cm_async_fd.readable().await;
                let mut guard = match guard_result {
                    Ok(g) => g,
                    Err(e) => break Some(Error::Verbs(e)),
                };

                match handle.event_channel.try_get_event() {
                    Ok(event) => {
                        let event_type = event.event_type();
                        let status = event.status();
                        tracing::debug!(?event_type, status, "driver: CM event");
                        match event_type {
                            CmEventType::Disconnected if status == 0 => break None,
                            CmEventType::Disconnected => {
                                // Nonzero status on disconnect indicates a failure
                                break Some(Error::Verbs(std::io::Error::other(format!(
                                    "CM disconnect with error status {status}"
                                ))));
                            }
                            CmEventType::DeviceRemoval => {
                                break Some(Error::Verbs(std::io::Error::other(
                                    "RDMA device removed",
                                )));
                            }
                            other => {
                                tracing::warn!(?other, status, "driver: unexpected CM event");
                                break Some(Error::Verbs(std::io::Error::other(format!(
                                    "unexpected CM event: {other:?} (status={status})"
                                ))));
                            }
                        }
                    }
                    Err(crate::Error::WouldBlock) => {
                        guard.clear_ready();
                        continue;
                    }
                    Err(e) => break Some(Error::from(e)),
                }
            }
        } else {
            // No CM handle — just pend forever
            std::future::pending::<Option<Error>>().await
        }
    };
    tokio::pin!(disconnect_future);

    // Track whether handshake completed successfully.
    let mut handshake_ok = false;

    'handshake: loop {
        // Check if we're done with handshake
        if hello_sent && peer_capacity.is_some() {
            handshake_ok = true;
            break;
        }

        // Check lifecycle: close/frontend-drop/terminal state
        if !state.frontend_alive.load(Ordering::Acquire)
            || state.state.load(Ordering::Acquire) >= STATE_CLOSING
        {
            break;
        }

        // Register shutdown interest BEFORE entering select (avoids lost wakeup)
        let shutdown_notified = state.state_notify.notified();
        tokio::pin!(shutdown_notified);

        // Re-check after registering
        if !state.frontend_alive.load(Ordering::Acquire)
            || state.state.load(Ordering::Acquire) >= STATE_CLOSING
        {
            break;
        }

        tokio::select! {
            biased;

            // Shutdown signal (close/drop) — exit handshake cleanly
            () = &mut shutdown_notified => {
                continue 'handshake; // re-evaluates loop-head checks
            }

            // Disconnect — exit handshake
            cm_error = &mut disconnect_future => {
                if let Some(e) = cm_error {
                    tracing::warn!("driver: CM error during handshake: {e}");
                    terminal_error = Some(e);
                } else {
                    tracing::info!("driver: peer disconnected during handshake");
                }
                break 'handshake;
            }

            // Drive CQ completions (always)
            Some(cq_result) = futures_util::StreamExt::next(&mut cq_futures) => {
                // A CQ driver exited during handshake — this is a problem
                match cq_result {
                    Ok(()) => terminal_error = Some(Error::DriverShutdown),
                    Err(e) => terminal_error = Some(e),
                }
                break 'handshake;
            }

            // Drive HELLO send if not done
            result = hello_send.as_mut(), if !hello_sent => {
                let (send_result, _mr_opt) = result;
                if let Err(e) = send_result {
                    terminal_error = Some(e);
                    break 'handshake;
                }
                hello_sent = true;
            }

            // Drive recv completions to catch peer's HELLO
            result = poll_any_ready(&mut remaining_futures), if peer_capacity.is_none() && !remaining_futures.is_empty() => {
                match result {
                    PollResult::Ready(idx, Ok((completion, mr))) => {
                        remaining_futures.swap_remove(idx);
                        let byte_len = completion.byte_len() as usize;
                        match protocol::parse_header(mr.as_slice(), byte_len) {
                            Ok(header) if header.frame_type == protocol::FRAME_HELLO => {
                                match protocol::parse_hello(
                                    &mr.as_slice()[protocol::HEADER_SIZE
                                        ..protocol::HEADER_SIZE + header.payload_len as usize],
                                ) {
                                    Ok(hello) => {
                                        let peer_max = hello.max_message_size as usize;
                                        if peer_max < buffer_size {
                                            terminal_error = Some(Error::ProtocolViolation(format!(
                                                "peer max_message_size {peer_max} < local buffer_size {buffer_size}; \
                                                 sending max-size messages would overrun peer recv buffers"
                                            )));
                                            break 'handshake;
                                        }
                                        // Validate peer recv capacity: must be >0
                                        // (zero → send() blocks forever) and must
                                        // fit in Semaphore::MAX_PERMITS (prevents
                                        // add_permits panic).
                                        let peer_cap = hello.data_recv_capacity as usize;
                                        if peer_cap == 0 {
                                            terminal_error = Some(Error::ProtocolViolation(
                                                "peer data_recv_capacity is 0".into(),
                                            ));
                                            break 'handshake;
                                        }
                                        if peer_cap > Semaphore::MAX_PERMITS {
                                            terminal_error = Some(Error::ProtocolViolation(format!(
                                                "peer data_recv_capacity {peer_cap} exceeds maximum ({max})",
                                                max = Semaphore::MAX_PERMITS,
                                            )));
                                            break 'handshake;
                                        }
                                        peer_capacity = Some(peer_cap);
                                        // Repost the HELLO recv buffer
                                        let qp = shared_qp.qp().clone();
                                        match post_recv_and_track(&qp, &recv_handle, mr) {
                                            Ok(future) => remaining_futures.push(future),
                                            Err(e) => {
                                                terminal_error = Some(e);
                                                break 'handshake;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        terminal_error = Some(e);
                                        break 'handshake;
                                    }
                                }
                            }
                            Ok(header) => {
                                terminal_error = Some(Error::ProtocolViolation(format!(
                                    "expected HELLO, got frame_type={}",
                                    header.frame_type,
                                )));
                                break 'handshake;
                            }
                            Err(e) => {
                                terminal_error = Some(e);
                                break 'handshake;
                            }
                        }
                    }
                    PollResult::Ready(idx, Err((err, _mr))) => {
                        remaining_futures.swap_remove(idx);
                        terminal_error = Some(err);
                        break 'handshake;
                    }
                }
            }

            // Timeout
            () = &mut hello_timeout => {
                terminal_error = Some(Error::ProtocolViolation("HELLO handshake timeout".into()));
                break 'handshake;
            }
        }
    }

    // Drop hello_send to release its inflight registry slot.
    // On failure paths where the OpFuture is still inflight, this pushes
    // the MR to the reclaim queue so the drain barrier can reach zero.
    drop(hello_send);

    // Take ownership of all pre-posted recv futures unconditionally.
    // On the happy path they become Phase B's pending_recvs; on every
    // failure/early-exit path they are dropped before the Phase C drain
    // so their inflight registry slots are released and the drain barrier
    // can reach zero.
    let mut pending_recvs = remaining_futures;

    if handshake_ok {
        // Initialize credits from peer capacity
        let credits = peer_capacity.unwrap();
        state.init_credits(credits);

        // Transition to Ready (use compare_exchange to detect concurrent close)
        let ready_ok = state
            .state
            .compare_exchange(
                STATE_CREATED,
                STATE_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();

        if ready_ok {
            state.state_notify.notify_waiters();

            // ─── Phase B: Steady State (recv pump + disconnect monitor + CQ drivers) ───

            // pending_recvs already holds the pre-posted recv futures
            // (taken unconditionally above).
            let mut pending_credits: u32 = 0;

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
                    let frame_len =
                        protocol::write_credit_frame(ctrl_mr.as_mut_slice(), credits_to_send);
                    let ctx = ctrl_send_tx.clone();
                    match post_send_and_detach(
                        shared_qp.qp(),
                        &send_handle,
                        ctrl_mr,
                        frame_len,
                        Box::new(move |mr| {
                            let _ = ctx.try_send(mr);
                        }),
                    ) {
                        Ok(()) => {
                            pending_credits = 0;
                        }
                        Err(e) => {
                            tracing::warn!("driver: CREDIT post failed: {e}");
                            if !matches!(e, Error::CapacityExhausted) {
                                terminal_error = Some(e);
                                break;
                            }
                        }
                    }
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
                                terminal_error = Some(e);
                            }
                            break;
                        }

                        // Disconnect
                        cm_error = &mut disconnect_future => {
                            if let Some(e) = cm_error {
                                tracing::warn!("driver: CM error: {e}");
                                terminal_error = Some(e);
                            } else {
                                tracing::info!("driver: peer disconnected");
                            }
                            break;
                        }

                        mr = repost_rx.recv() => {
                            match mr {
                                Some(mr) => {
                                    pending_credits += 1;
                                    match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                        Ok(future) => pending_recvs.push(future),
                                        Err(e) => {
                                            terminal_error = Some(e);
                                            break;
                                        }
                                    }
                                }
                                None => break, // frontend dropped repost sender
                            }
                            continue;
                        }

                        ctrl_mr = ctrl_send_rx.recv(), if pending_credits > 0 => {
                            if let Some(mut ctrl_mr) = ctrl_mr {
                                let credits_to_send = pending_credits;
                                let frame_len = protocol::write_credit_frame(ctrl_mr.as_mut_slice(), credits_to_send);
                                let ctx = ctrl_send_tx.clone();
                                match post_send_and_detach(shared_qp.qp(), &send_handle, ctrl_mr, frame_len, Box::new(move |mr| { let _ = ctx.try_send(mr); })) {
                                    Ok(()) => {
                                        pending_credits = 0;
                                    }
                                    Err(e) => {
                                        tracing::warn!("driver: CREDIT post failed: {e}");
                                        if !matches!(e, Error::CapacityExhausted) {
                                            terminal_error = Some(e);
                                            break;
                                        }
                                    }
                                }
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
                                terminal_error = Some(e);
                            }
                            break;
                        }

                        // Disconnect
                        cm_error = &mut disconnect_future => {
                            if let Some(e) = cm_error {
                                tracing::warn!("driver: CM error: {e}");
                                terminal_error = Some(e);
                            } else {
                                tracing::info!("driver: peer disconnected");
                            }
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
                                                    match protocol::parse_credit(
                                                        &mr.as_slice()[protocol::HEADER_SIZE
                                                            ..protocol::HEADER_SIZE + header.payload_len as usize],
                                                    ) {
                                                        Ok(credit) => {
                                                            if let Err(e) = state.validate_and_add_credits(credit.credits) {
                                                                tracing::warn!("driver: credit validation failed: {e}");
                                                                terminal_error = Some(e);
                                                                break;
                                                            }
                                                            match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                                                Ok(future) => pending_recvs.push(future),
                                                                Err(e) => {
                                                                    terminal_error = Some(e);
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            tracing::warn!("driver: malformed CREDIT frame: {e}");
                                                            terminal_error = Some(e);
                                                            break;
                                                        }
                                                    }
                                                }
                                                protocol::FRAME_HELLO => {
                                                    // Unexpected HELLO after handshake is a protocol violation
                                                    terminal_error = Some(Error::ProtocolViolation(
                                                        "unexpected HELLO frame during steady-state operation".into(),
                                                    ));
                                                    break;
                                                }
                                                _ => {
                                                    tracing::warn!(frame_type = header.frame_type, "driver: unknown frame type");
                                                    match post_recv_and_track(shared_qp.qp(), &recv_handle, mr) {
                                                        Ok(future) => pending_recvs.push(future),
                                                        Err(e) => {
                                                            terminal_error = Some(e);
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("driver: protocol parse error: {e}");
                                            terminal_error = Some(e);
                                            break;
                                        }
                                    }
                                }
                                PollResult::Ready(idx, Err((err, _mr))) => {
                                    pending_recvs.swap_remove(idx);
                                    // WrFlushErr during shutdown/disconnect is expected,
                                    // not a driver error (same mapping as send()).
                                    match &err {
                                        Error::CompletionError { status, .. }
                                            if *status == crate::wc::WcStatus::WrFlushErr => {
                                            tracing::debug!("driver: recv flush error during shutdown");
                                        }
                                        _ => {
                                            tracing::debug!("driver: recv completion error: {err}");
                                            terminal_error = Some(err);
                                        }
                                    }
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
                                        Err(e) => {
                                            terminal_error = Some(e);
                                            break;
                                        }
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                }
            }
        } // end else (state compare_exchange ok → Phase B ran)
    } // end if (handshake_ok → tried Phase B)

    // ─── Phase C: Shutdown ───

    // Close credits first to unblock any waiting frontend operations
    state.remote_credits.close();

    // Initiate QP shutdown — transitions QP to error state, causing the HCA
    // to generate flush CQEs for all outstanding WRs.
    if let Err(e) = shared_qp.shutdown() {
        tracing::warn!("driver: QP shutdown failed during Phase C: {e}");
    }

    // Signal CQ drivers to enter their final drain barrier. We use
    // shutdown() (not close_and_shutdown) to let real flush CQEs arrive
    // from the hardware rather than writing synthetic completions.
    // This preserves the teardown invariant: an MR may only be returned
    // after its real CQE is reaped or the QP is destroyed.
    for h in &driver_handles {
        h.shutdown();
    }

    // Drop pending recv OpFutures — pushes their MRs to the reclaim queue
    // for safe cleanup during the CQ drain barrier.
    drop(pending_recvs);
    drop(recv_msg_tx);

    // Let CQ drivers drain their final barrier (processes real flush CQEs).
    let drain_timeout = tokio::time::sleep(DRAIN_TIMEOUT);
    tokio::pin!(drain_timeout);
    while !cq_futures.is_empty() {
        tokio::select! {
            biased;
            Some(_) = futures_util::StreamExt::next(&mut cq_futures) => {}
            () = &mut drain_timeout => {
                tracing::warn!("driver: CQ drain timed out — quarantining remaining MRs");
                break;
            }
        }
    }

    // Close inflight maps to wake any remaining OpFuture waiters. They
    // will quarantine their MRs (push to reclaim queue), which are freed
    // only after QP destruction per ConnectionLifetime field ordering.
    //
    // Close unconditionally: even on clean drain exit, the CQ drivers have
    // stopped and no further real CQEs can arrive. Any remaining occupied
    // slots (e.g., from hello_send on failure paths) must be closed so
    // their waiters don't hang.
    for h in &driver_handles {
        h.map().close();
    }

    // Now transition to terminal state and notify waiters
    // (AFTER drain is complete, so close().await means drain is done)
    if let Some(ref err) = terminal_error {
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

    // Connection lifetime lease (conn_lifetime) is dropped when this
    // function returns. Combined with the frontend's lease via
    // TransportSharedState, the last holder's drop runs
    // ConnectionLifetime's destructor in safe order:
    // SharedQp (QP destroy) → Pd → CmId → EventChannel.

    if let Some(err) = terminal_error {
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
        std::result::Result<(super::op::Completion, Mr), (Error, Option<Mr>)>,
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
                    Ok(completion) => {
                        // Real CQE: mr is always Some
                        Ok((completion, mr.expect("real CQE must return MR")))
                    }
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

    let reg = handle.map().register().ok_or_else(|| {
        if handle.map().is_closed() {
            Error::DriverShutdown
        } else {
            Error::CapacityExhausted
        }
    })?;
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
    let reg = match handle.map().register() {
        Some(r) => r,
        None => {
            // Return MR to pool via callback on registry exhaustion
            on_reclaim(mr);
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
mod setup_tests {
    use super::*;
    use crate::v2::engine::{BatchOwnershipTransfer, PreparedBatchOwnership};
    use crate::v2::qp::BatchPostOutcome;

    #[test]
    fn checked_message_capacity_derivation_and_default_headroom_are_exact() {
        let config = MessageTransportBuilder::new()
            .derive_engine_config()
            .unwrap();
        assert_eq!(config.connection.maximum_send_work_requests(), 19);
        assert_eq!(config.connection.maximum_receive_work_requests(), 34);
        assert_eq!(
            config.connection.maximum_send_work_requests()
                + config.connection.maximum_receive_work_requests(),
            53
        );
        assert_eq!(256 * 53, 13_568);
        assert_eq!(16_384 - 13_568, 2_816);

        let minimum = MessageTransportBuilder::new()
            .send_buffers(1)
            .recv_buffers(1)
            .derive_engine_config()
            .unwrap();
        assert_eq!(minimum.connection.maximum_send_work_requests(), 4);
        assert_eq!(minimum.connection.maximum_receive_work_requests(), 3);
    }

    #[test]
    fn explicit_connection_config_may_exceed_but_not_undershoot_derivation() {
        let exact = RdmaConnectionConfig::default()
            .max_send_wr(7)
            .max_recv_wr(10);
        let config = MessageTransportBuilder::new()
            .send_buffers(4)
            .recv_buffers(8)
            .connection_config(exact.clone())
            .derive_engine_config()
            .unwrap();
        assert_eq!(config.connection, exact);

        let larger = RdmaConnectionConfig::default()
            .max_send_wr(8)
            .max_recv_wr(11);
        assert!(
            MessageTransportBuilder::new()
                .send_buffers(4)
                .recv_buffers(8)
                .connection_config(larger)
                .derive_engine_config()
                .is_ok()
        );
        for insufficient in [
            RdmaConnectionConfig::default()
                .max_send_wr(6)
                .max_recv_wr(10),
            RdmaConnectionConfig::default()
                .max_send_wr(7)
                .max_recv_wr(9),
        ] {
            assert!(matches!(
                MessageTransportBuilder::new()
                    .send_buffers(4)
                    .recv_buffers(8)
                    .connection_config(insufficient)
                    .derive_engine_config(),
                Err(Error::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn zero_and_overflow_inputs_fail_before_engine_establishment() {
        for builder in [
            MessageTransportBuilder::new().send_buffers(0),
            MessageTransportBuilder::new().recv_buffers(0),
            MessageTransportBuilder::new().buffer_size(0),
            MessageTransportBuilder::new().send_buffers(usize::MAX),
            MessageTransportBuilder::new().recv_buffers(usize::MAX),
        ] {
            assert!(matches!(
                builder.derive_engine_config(),
                Err(Error::InvalidConfig(_))
            ));
        }
        if usize::BITS > 32 {
            let builder = MessageTransportBuilder::new().buffer_size(u32::MAX as usize + 1);
            assert!(matches!(
                builder.derive_engine_config(),
                Err(Error::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn accepted_prefix_and_ambiguous_setup_ownership_are_exact() {
        let count = 5;
        for first_unaccepted in 0..count {
            let transfer = PreparedBatchOwnership::new((0..count).collect::<Vec<_>>())
                .unwrap()
                .consume(BatchPostOutcome::PrefixAccepted {
                    accepted: first_unaccepted,
                    first_unaccepted,
                    source: std::io::Error::from_raw_os_error(libc::ENOMEM),
                });
            let BatchOwnershipTransfer::Partial {
                accepted,
                unaccepted,
                ..
            } = transfer
            else {
                panic!("valid bad_wr membership must produce an exact split");
            };
            assert_eq!(accepted, (0..first_unaccepted).collect::<Vec<_>>());
            assert_eq!(unaccepted, (first_unaccepted..count).collect::<Vec<_>>());
        }

        let accepted = PreparedBatchOwnership::new((0..count).collect::<Vec<_>>())
            .unwrap()
            .consume(BatchPostOutcome::AllAccepted);
        assert!(matches!(
            accepted,
            BatchOwnershipTransfer::Accepted(entries) if entries.len() == count
        ));

        let ambiguous = PreparedBatchOwnership::new((0..count).collect::<Vec<_>>())
            .unwrap()
            .consume(BatchPostOutcome::Ambiguous {
                source: std::io::Error::from_raw_os_error(libc::EIO),
            });
        assert!(matches!(
            ambiguous,
            BatchOwnershipTransfer::Ambiguous { retained, .. }
                if retained.len() == count
        ));
    }
}

#[cfg(test)]
mod hello_tests {
    use super::*;
    use crate::v2::engine::{
        DEFAULT_MESSAGE_HELLO_DEADLINE, MAX_MESSAGE_HELLO_DEADLINE, MIN_MESSAGE_HELLO_DEADLINE,
    };

    fn hello(capacity: u32, maximum: u32) -> protocol::HelloPayload {
        protocol::HelloPayload {
            data_recv_capacity: capacity,
            max_message_size: maximum,
            protocol_version: protocol::PROTO_VERSION as u32,
        }
    }

    #[test]
    fn hello_deadline_default_and_bounds_are_exact() {
        assert_eq!(DEFAULT_MESSAGE_HELLO_DEADLINE, Duration::from_secs(10));
        assert_eq!(MIN_MESSAGE_HELLO_DEADLINE, Duration::from_millis(1));
        assert_eq!(MAX_MESSAGE_HELLO_DEADLINE, Duration::from_secs(5 * 60));
    }

    #[test]
    fn hello_negotiates_exact_peer_receive_credits_and_message_boundary() {
        assert_eq!(validate_peer_hello(hello(32, 65_536), 65_536).unwrap(), 32);
        assert!(matches!(
            validate_peer_hello(hello(0, 65_536), 65_536),
            Err(Error::ProtocolViolation(_))
        ));
        assert!(matches!(
            validate_peer_hello(hello(32, 65_535), 65_536),
            Err(Error::ProtocolViolation(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn hello_timeout_is_contextual_and_wakes_ready_waiters() {
        let config = MessageTransportBuilder::new()
            .derive_engine_config()
            .unwrap();
        let state = Arc::new(EngineMessageState::new(&config));
        let ready_state = Arc::clone(&state);
        let ready = tokio::spawn(async move { ready_state.ready().await });
        tokio::task::yield_now().await;
        state.deadline_expired();
        let error = ready.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            Error::ProtocolViolation(message) if message == "HELLO handshake timeout"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Credit Validation Unit Tests ──────────────────────────────────
    //
    // All credit tests exercise the production `check_credit_return()`
    // function directly. No duplicate logic.

    #[test]
    fn test_credit_exact_capacity_return() {
        // Capacity 4, all 4 in flight, available=0 → return all 4.
        assert!(check_credit_return(4, 4, 0, 4).is_ok());
    }

    #[test]
    fn test_credit_partial_return() {
        // Capacity 4, 3 in flight, available=1 → return 2.
        assert!(check_credit_return(2, 3, 1, 4).is_ok());
    }

    #[test]
    fn test_credit_duplicate_return_rejected() {
        // Capacity 4, 0 in flight, available=4 → return 1 → exceeds in_flight.
        let err = check_credit_return(1, 0, 4, 4).unwrap_err();
        assert!(
            err.to_string().contains("exceeds in-flight"),
            "expected in-flight error, got: {err}"
        );
    }

    #[test]
    fn test_credit_overflow_large_count() {
        // Capacity 4, 0 in flight, available=4 → return 100 → exceeds.
        let err = check_credit_return(100, 0, 4, 4).unwrap_err();
        assert!(err.to_string().contains("exceeds in-flight"));
    }

    #[test]
    fn test_credit_overflow_u32_max() {
        // Capacity 4, 4 in flight, available=0 → return u32::MAX → exceeds.
        let err = check_credit_return(u32::MAX, 4, 0, 4).unwrap_err();
        assert!(err.to_string().contains("exceeds in-flight"));
    }

    #[test]
    fn test_credit_zero_count_rejected() {
        let err = check_credit_return(0, 4, 0, 4).unwrap_err();
        assert!(
            err.to_string().contains("zero credits"),
            "expected zero-credits error, got: {err}"
        );
    }

    #[test]
    fn test_credit_boundary_exactly_at_cap() {
        // available=3, in_flight=1, capacity=4 → return 1 → exactly at cap.
        assert!(check_credit_return(1, 1, 3, 4).is_ok());
        // Then: available=4, in_flight=0 → return 1 → exceeds.
        let err = check_credit_return(1, 0, 4, 4).unwrap_err();
        assert!(err.to_string().contains("exceeds in-flight"));
    }

    #[test]
    fn test_credit_multiple_partial_returns() {
        // Capacity 8, 6 in flight, available=2.
        // Return 2: ok. Remaining: in_flight=4, available=4.
        assert!(check_credit_return(2, 6, 2, 8).is_ok());
        // Return 2: ok. Remaining: in_flight=2, available=6.
        assert!(check_credit_return(2, 4, 4, 8).is_ok());
        // Return 2: ok. Remaining: in_flight=0, available=8.
        assert!(check_credit_return(2, 2, 6, 8).is_ok());
        // Return 1: exceeds in_flight=0.
        let err = check_credit_return(1, 0, 8, 8).unwrap_err();
        assert!(err.to_string().contains("exceeds in-flight"));
    }

    #[test]
    fn test_credit_in_flight_check_catches_toctou() {
        // Scenario: capacity=4, available=3 (1 acquired but NOT forgotten).
        // A bogus CREDIT(1) would pass the old capacity-only check (3+1=4),
        // but the in-flight check catches it: 0 in flight, can't return 1.
        let err = check_credit_return(1, 0, 3, 4).unwrap_err();
        assert!(
            err.to_string().contains("exceeds in-flight"),
            "in-flight check should catch bogus credit during acquire window"
        );
    }

    #[test]
    fn test_credit_capacity_check_catches_overflow() {
        // available=usize::MAX-1, in_flight=usize::MAX, capacity=4 → return 2.
        // In-flight check passes (2 <= usize::MAX), but capacity check catches
        // the overflow via saturating_add.
        let err = check_credit_return(2, usize::MAX, usize::MAX - 1, 4).unwrap_err();
        assert!(
            err.to_string().contains("exceed negotiated capacity"),
            "capacity check should catch arithmetic overflow"
        );
    }

    #[test]
    fn test_credit_error_is_protocol_violation() {
        use crate::v2::error::TransportErrorKind;
        let err = check_credit_return(0, 4, 0, 4).unwrap_err();
        let te = TransportError::from_error(&err);
        assert_eq!(*te.kind(), TransportErrorKind::ProtocolViolation);

        let err = check_credit_return(10, 0, 4, 4).unwrap_err();
        let te = TransportError::from_error(&err);
        assert_eq!(*te.kind(), TransportErrorKind::ProtocolViolation);
    }

    #[test]
    fn test_credit_in_flight_valid_range() {
        // in_flight exactly matches credits → ok.
        assert!(check_credit_return(3, 3, 1, 4).is_ok());
        // in_flight exceeds credits → ok (partial return).
        assert!(check_credit_return(1, 3, 1, 4).is_ok());
        // credits exceeds in_flight → error.
        assert!(check_credit_return(4, 3, 0, 4).is_err());
    }

    // ── Builder Tests ─────────────────────────────────────────────────

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
        // max_recv_wr = recv_count + ctrl_recv_count = 8 + 2 = 10
        assert_eq!(cfg.max_recv_wr, 10);
        // total = 7 + 10 = 17; inflight = 17
        assert_eq!(cfg.inflight_capacity, 17);
    }
}
