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
//! additional message tasks. There is no dedicated receive pump or disconnect
//! monitor.
//!
//! ```no_run
//! # use rdma_io::v2::*;
//! # async fn example() -> Result<()> {
//! let (engine, driver) = RdmaEngineBuilder::new("rxe0").build()?;
//! let driver_task = tokio::spawn(driver);
//! let transport = MessageTransportBuilder::new()
//!     .connect_on(&engine, "192.168.1.1:7471".parse().unwrap())
//!     .await?;
//!
//! transport.ready().await?;
//! transport.send(b"hello").await?;
//! let msg = transport.recv().await?;
//! assert_eq!(msg.as_ref(), b"hello");
//! transport.close().await?;
//! engine.shutdown().await?;
//! driver_task.await.expect("driver task panicked")?;
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
//! engine-ready work increments `credits_in_flight` immediately after
//! forgetting the permit and before offering the DATA WR to the provider.
//! Provider-proven unaccepted WRs roll back with checked atomic subtraction;
//! accepted or ambiguous WRs retain their accounting until the exact CQE.
//!
//! The peer's `data_recv_capacity` announced during HELLO is validated
//! (must be > 0 and ≤ `Semaphore::MAX_PERMITS`) before credit
//! initialization.
//!
//! A violating CREDIT triggers [`Error::ProtocolViolation`], closes the
//! connection through normal engine progress, and is returned by transport
//! operations and [`MessageTransport::close`].
//!
//! # Receive-Buffer Invariant
//!
//! Every configured receive MR is in exactly one of these states at any time:
//! - **Posted** — registered in the engine operation registry and posted to
//!   the QP as a receive WR
//! - **Delivered** — completed by the HCA, queued internally or held
//!   by a [`ReceivedMessage`] handle
//! - **Queued for repost** — returned by [`ReceivedMessage::drop`] to
//!   connection-local engine work
//! - **Teardown-owned** — transport is shutting down; the MR will be dropped
//!
//! # Disconnect Monitoring
//!
//! The sole engine driver consumes CM events and completion errors. Peer
//! disconnect and ordinary `WrFlushErr` teardown on HELLO, receive, and
//! steady-state paths are normalized to [`Error::TransportClosed`]. Other CM,
//! protocol, and provider failures retain their contextual [`Error`], wake
//! pending waiters, and initiate the same explicit local QP-to-ERR drain.
//! Errors are observed from `ready`, `send`, `recv`, and `close`; there is no
//! separate public error accessor.

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::task::{Context, Poll};
#[cfg(any(test, feature = "test-hooks"))]
use std::time::Duration;

use futures_util::task::AtomicWaker;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use super::engine::{
    ConnectionReadyWork, ConnectionState, DetachedOperationCompletion, EngineShared,
    PreEstablishSetup, RdmaConnection, RdmaConnectionConfig, RdmaEngine, RdmaListener,
    SetupSummary,
};
use super::error::{Error, Result};
use super::mr::{AccessIntent, Mr};
use super::protocol;

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
///
/// The default QP requirement is exactly 19 send WRs
/// (`16 + 2 control + 1 HELLO`) and 34 receive WRs
/// (`32 + 2 control`). All 34 receives are posted before `rdma_connect` or
/// `rdma_accept`; HELLO reuses one control receive rather than adding another.
///
/// # Errors
///
/// Builder methods do not fail. Validation occurs at `connect_on()` or
/// `accept_on()` time; invalid configuration produces [`Error::InvalidConfig`].
pub struct MessageTransportBuilder {
    recv_buffer_count: usize,
    send_buffer_count: usize,
    buffer_size: usize,
    connection_config: Option<RdmaConnectionConfig>,
    #[cfg(any(test, feature = "test-hooks"))]
    hello_override: Option<TestHelloOverride>,
    #[cfg(any(test, feature = "test-hooks"))]
    attach_hook: Option<TestHelloAttachHook>,
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
            connection_config: None,
            #[cfg(any(test, feature = "test-hooks"))]
            hello_override: None,
            #[cfg(any(test, feature = "test-hooks"))]
            attach_hook: None,
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

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_hello_attach_hook(mut self, hook: TestHelloAttachHook) -> Self {
        self.attach_hook = Some(hook);
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
                if config.max_send_wr < required_send_wr {
                    return Err(Error::InvalidConfig(format!(
                        "connection maximum send WRs ({}) is below the message requirement ({required_send_wr})",
                        config.max_send_wr
                    )));
                }
                if config.max_recv_wr < required_recv_wr {
                    return Err(Error::InvalidConfig(format!(
                        "connection maximum receive WRs ({}) is below the message requirement ({required_recv_wr})",
                        config.max_recv_wr
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
            #[cfg(any(test, feature = "test-hooks"))]
            attach_hook: self.attach_hook.clone(),
        })
    }

    /// Attach an outbound message transport to an RDMA engine.
    ///
    /// The returned frontend has no connection-local driver. The engine driver
    /// owns receive pre-posting, HELLO negotiation, CQ routing, and readiness.
    /// The full checked receive batch is accepted before `rdma_connect`; setup
    /// failure follows exact `bad_wr` prefix/suffix/ambiguity ownership.
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
    /// accept queue and returns no listener- or message-specific driver. The
    /// full checked receive batch is accepted before `rdma_accept`.
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
}

struct EngineMessageConfig {
    connection: RdmaConnectionConfig,
    send_count: usize,
    recv_count: usize,
    buffer_size: usize,
    mr_size: usize,
    #[cfg(any(test, feature = "test-hooks"))]
    hello_override: Option<TestHelloOverride>,
    #[cfg(any(test, feature = "test-hooks"))]
    attach_hook: Option<TestHelloAttachHook>,
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

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum TestSteadyFrame {
    Credit(u32),
    Hello,
    BadMagicData,
    TrailingDataByte,
    TruncatedDataPayload,
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone, Default)]
#[doc(hidden)]
pub struct TestHelloAttachHook {
    inner: Arc<TestHelloAttachHookInner>,
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Default)]
struct TestHelloAttachHookInner {
    state: StdMutex<TestHelloAttachHookState>,
    changed: Condvar,
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Default)]
struct TestHelloAttachHookState {
    ready_work_attached: bool,
    hello_processed: bool,
    released: bool,
    message_state: Option<Weak<EngineMessageState>>,
    hello_mr: Option<Mr>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl TestHelloAttachHook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn wait_until_ready_work_attached(&self) -> Result<()> {
        self.wait_until(
            |state| state.ready_work_attached,
            "message ready work attachment",
        )
    }

    pub fn wait_until_hello_processed(&self) -> Result<()> {
        self.wait_until(
            |state| state.hello_processed,
            "HELLO processing during message attachment",
        )
    }

    pub fn deliver_hello(&self) -> Result<()> {
        let (state, mut mr) = {
            let mut hook = lock_std(&self.inner.state);
            let state = hook
                .message_state
                .clone()
                .and_then(|state| state.upgrade())
                .ok_or_else(|| {
                    Error::InvalidConfig(
                        "message ready work is not attached for HELLO delivery".into(),
                    )
                })?;
            let mr = hook.hello_mr.take().ok_or_else(|| {
                Error::InvalidConfig("test HELLO receive MR is unavailable".into())
            })?;
            (state, mr)
        };
        if !state
            .weak_self()
            .upgrade()
            .is_some_and(|self_state| Arc::ptr_eq(&self_state, &state))
        {
            return Err(Error::InvalidConfig(
                "message self reference was not initialized before HELLO delivery".into(),
            ));
        }
        drop(state.connection()?);
        let len = protocol::write_hello_frame(
            mr.as_mut_slice(),
            u32::try_from(state.local_recv_capacity)
                .map_err(|_| Error::InvalidConfig("test HELLO capacity overflow".into()))?,
            u32::try_from(state.buffer_size)
                .map_err(|_| Error::InvalidConfig("test HELLO size overflow".into()))?,
        );
        let mut completion = crate::wc::WorkCompletion::default();
        completion.inner.status = rdma_io_sys::ibverbs::IBV_WC_SUCCESS;
        completion.inner.opcode = rdma_io_sys::ibverbs::IBV_WC_RECV;
        completion.inner.byte_len = u32::try_from(len)
            .map_err(|_| Error::InvalidConfig("test HELLO length overflow".into()))?;
        state.parse_hello_receive(&super::op::Completion::from_raw(completion), &mr)?;
        self.record_hello_processed();
        Ok(())
    }

    fn prepare_hello_mr(&self, mr: Mr) -> Result<()> {
        let mut state = lock_std(&self.inner.state);
        if state.hello_mr.is_some() {
            return Err(Error::InvalidConfig(
                "test HELLO receive MR is already prepared".into(),
            ));
        }
        state.hello_mr = Some(mr);
        Ok(())
    }

    pub fn release(&self) {
        let mut state = lock_std(&self.inner.state);
        state.released = true;
        self.inner.changed.notify_all();
    }

    fn pause_after_ready_work_attach(&self, message_state: &Arc<EngineMessageState>) {
        let mut state = lock_std(&self.inner.state);
        state.message_state = Some(Arc::downgrade(message_state));
        state.ready_work_attached = true;
        self.inner.changed.notify_all();
        while !state.released {
            let (next, timeout) = self
                .inner
                .changed
                .wait_timeout(state, Duration::from_secs(15))
                .unwrap_or_else(|error| error.into_inner());
            state = next;
            if timeout.timed_out() {
                state.released = true;
            }
        }
    }

    fn record_hello_processed(&self) {
        let mut state = lock_std(&self.inner.state);
        state.hello_processed = true;
        self.inner.changed.notify_all();
    }

    fn wait_until(
        &self,
        mut predicate: impl FnMut(&TestHelloAttachHookState) -> bool,
        description: &str,
    ) -> Result<()> {
        let mut state = lock_std(&self.inner.state);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !predicate(&state) {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(Error::InvalidConfig(format!(
                    "timed out waiting for {description}"
                )));
            };
            let (next, timeout) = self
                .inner
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
            if timeout.timed_out() && !predicate(&state) {
                return Err(Error::InvalidConfig(format!(
                    "timed out waiting for {description}"
                )));
            }
        }
        Ok(())
    }
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
        let mut data_sends = Vec::with_capacity(self.state.local_send_capacity);
        for _ in 0..self.state.local_send_capacity {
            data_sends.push(connection.register_memory(self.mr_size, AccessIntent::LocalOnly)?);
        }
        let mut control_sends = Vec::with_capacity(protocol::CTRL_SEND_COUNT);
        for _ in 0..protocol::CTRL_SEND_COUNT {
            control_sends.push(
                connection.register_memory(protocol::CTRL_BUF_SIZE, AccessIntent::LocalOnly)?,
            );
        }
        let hello_send =
            connection.register_memory(protocol::HELLO_FRAME_SIZE, AccessIntent::LocalOnly)?;
        self.state
            .install_pools(data_sends, control_sends, hello_send)?;
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(hook) = self.state.attach_hook.as_ref() {
            hook.prepare_hello_mr(
                connection.register_memory(self.mr_size, AccessIntent::LocalOnly)?,
            )?;
        }
        let mut entries = Vec::with_capacity(total);
        for _ in 0..total {
            let mr = connection.register_memory(self.mr_size, AccessIntent::LocalOnly)?;
            let state = Arc::downgrade(&self.state);
            entries.push((
                mr,
                Box::new(move |completion| {
                    if let Some(state) = state.upgrade() {
                        state.enqueue_event(EngineMessageEvent::Receive(completion));
                    } else {
                        // The callback owns the completion and its MR; dropping
                        // it here is the only release path after state teardown.
                        drop(completion);
                    }
                }) as _,
            ));
        }
        let posted = connection.post_detached_recv_batch(entries)?;
        Ok(SetupSummary { posted_wrs: posted })
    }
}

/// A received message with its exact byte length.
///
/// Wraps a registered MR and exposes only the received payload (after
/// the protocol header). When dropped, the backing MR is returned to
/// the transport's receive pool for reposting, which also sends a
/// CREDIT frame to the peer.
///
/// `ReceivedMessage` implements `AsRef<[u8]>` and
/// `Deref<Target = [u8]>`. Both views are exactly the application payload:
/// they exclude the internal wire header and have length [`Self::len`].
///
/// # Cancellation Safety
///
/// Dropping a `ReceivedMessage` safely returns its buffer for reposting.
/// Drop briefly enqueues connection-local engine work and publishes a driver
/// wake; it does not await or run a receive-pump task.
pub struct ReceivedMessage {
    mr: Option<Mr>,
    byte_len: usize,
    repost: Weak<EngineMessageState>,
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
        if let Some(mr) = self.mr.take()
            && let Some(state) = self.repost.upgrade()
        {
            state.enqueue_event(EngineMessageEvent::Repost(mr));
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

fn transition_terminal(state: &AtomicU8, target: u8) -> bool {
    debug_assert!(matches!(target, STATE_STOPPED | STATE_FAILED));
    let mut current = state.load(Ordering::Acquire);
    loop {
        if matches!(current, STATE_STOPPED | STATE_FAILED) {
            return false;
        }
        match state.compare_exchange_weak(current, target, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

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

fn claim_pending_credit_returns(pending: &AtomicUsize) -> usize {
    let mut observed = pending.load(Ordering::Acquire);
    loop {
        if observed == 0 {
            return 0;
        }
        let claimed = observed.min(u32::MAX as usize);
        match pending.compare_exchange_weak(
            observed,
            observed - claimed,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return claimed,
            Err(current) => observed = current,
        }
    }
}

/// Internal struct for a completed receive to pass through the channel.
struct CompletedRecv {
    mr: Mr,
    byte_len: usize,
}

struct RegisteredMrPool {
    inner: StdMutex<RegisteredMrPoolInner>,
    available: Arc<Semaphore>,
}

struct RegisteredMrPoolInner {
    closed: bool,
    entries: Vec<Mr>,
}

impl RegisteredMrPool {
    fn new(entries: Vec<Mr>) -> Arc<Self> {
        Arc::new(Self {
            available: Arc::new(Semaphore::new(entries.len())),
            inner: StdMutex::new(RegisteredMrPoolInner {
                closed: false,
                entries,
            }),
        })
    }

    async fn take(self: &Arc<Self>) -> Result<Mr> {
        let permit = Arc::clone(&self.available)
            .acquire_owned()
            .await
            .map_err(|_| Error::TransportClosed)?;
        let mut inner = lock_std(&self.inner);
        if inner.closed {
            return Err(Error::TransportClosed);
        }
        let mr = inner
            .entries
            .pop()
            .ok_or_else(|| Error::InvalidConfig("registered MR pool accounting mismatch".into()))?;
        permit.forget();
        Ok(mr)
    }

    fn try_take(self: &Arc<Self>) -> Option<Mr> {
        let permit = Arc::clone(&self.available).try_acquire_owned().ok()?;
        let mut inner = lock_std(&self.inner);
        if inner.closed {
            return None;
        }
        let mr = inner.entries.pop()?;
        permit.forget();
        Some(mr)
    }

    fn put(&self, mr: Mr) {
        let mut inner = lock_std(&self.inner);
        if inner.closed {
            drop(inner);
            drop(mr);
            return;
        }
        inner.entries.push(mr);
        drop(inner);
        self.available.add_permits(1);
    }

    fn close(&self) {
        let entries = {
            let mut inner = lock_std(&self.inner);
            if inner.closed {
                return;
            }
            inner.closed = true;
            self.available.close();
            std::mem::take(&mut inner.entries)
        };
        drop(entries);
    }

    fn available(&self) -> usize {
        lock_std(&self.inner).entries.len()
    }
}

struct EngineMessagePools {
    data_sends: Arc<RegisteredMrPool>,
    control_sends: Arc<RegisteredMrPool>,
    hello_send: StdMutex<Option<Mr>>,
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
    HelloSend(DetachedOperationCompletion),
    Receive(DetachedOperationCompletion),
    Repost(Mr),
    SendRequest(Arc<EngineSendRequest>),
    SendComplete {
        request: Arc<EngineSendRequest>,
        completion: DetachedOperationCompletion,
    },
    ControlSendComplete(DetachedOperationCompletion),
}

struct EngineSendRequest {
    inner: StdMutex<EngineSendRequestInner>,
    cancelled: AtomicBool,
    credit_committed: AtomicBool,
    waker: AtomicWaker,
}

struct EngineSendRequestInner {
    mr: Option<Mr>,
    credit: Option<OwnedSemaphorePermit>,
    frame_len: usize,
    output: Option<Result<()>>,
}

enum EngineSendRequestAction {
    Post {
        mr: Mr,
        credit: Option<OwnedSemaphorePermit>,
        frame_len: usize,
    },
    Cancelled {
        mr: Mr,
        credit: Option<OwnedSemaphorePermit>,
    },
    AlreadyHandled,
}

impl EngineSendRequest {
    fn new(mr: Mr, credit: Option<OwnedSemaphorePermit>, frame_len: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: StdMutex::new(EngineSendRequestInner {
                mr: Some(mr),
                credit,
                frame_len,
                output: None,
            }),
            cancelled: AtomicBool::new(false),
            credit_committed: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        })
    }

    fn start(&self) -> EngineSendRequestAction {
        let mut inner = lock_std(&self.inner);
        let Some(mr) = inner.mr.take() else {
            return EngineSendRequestAction::AlreadyHandled;
        };
        let credit = inner.credit.take();
        if self.cancelled.load(Ordering::Acquire) {
            return EngineSendRequestAction::Cancelled { mr, credit };
        }
        EngineSendRequestAction::Post {
            mr,
            credit,
            frame_len: inner.frame_len,
        }
    }

    fn complete(&self, result: Result<()>) {
        let mut inner = lock_std(&self.inner);
        if inner.output.is_none() {
            inner.output = Some(result);
        }
        drop(inner);
        self.waker.wake();
    }

    fn take_output(&self) -> Option<Result<()>> {
        lock_std(&self.inner).output.take()
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct EngineSendWaiter {
    request: Arc<EngineSendRequest>,
    state: Weak<EngineMessageState>,
    done: bool,
}

impl Future for EngineSendWaiter {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.request.take_output() {
            self.done = true;
            return Poll::Ready(result);
        }
        self.request.waker.register(cx.waker());
        if let Some(result) = self.request.take_output() {
            self.done = true;
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}

impl Drop for EngineSendWaiter {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.request.cancel();
        if let Some(state) = self.state.upgrade() {
            state.publish();
        }
    }
}

struct EngineMessageState {
    state: AtomicU8,
    state_notify: Notify,
    error: StdMutex<Option<Error>>,
    remote_credits: Arc<Semaphore>,
    peer_recv_capacity: AtomicUsize,
    credits_in_flight: AtomicUsize,
    local_recv_capacity: usize,
    local_send_capacity: usize,
    buffer_size: usize,
    pools: OnceLock<EngineMessagePools>,
    handshake: StdMutex<EngineHandshake>,
    events: StdMutex<VecDeque<EngineMessageEvent>>,
    received: StdMutex<VecDeque<CompletedRecv>>,
    recv_notify: Notify,
    pending_credit_returns: AtomicUsize,
    link: OnceLock<EngineMessageLink>,
    self_weak: OnceLock<Weak<EngineMessageState>>,
    #[cfg(any(test, feature = "test-hooks"))]
    hello_override: Option<TestHelloOverride>,
    #[cfg(any(test, feature = "test-hooks"))]
    attach_hook: Option<TestHelloAttachHook>,
}

impl EngineMessageState {
    fn new(config: &EngineMessageConfig) -> Self {
        Self {
            state: AtomicU8::new(STATE_CREATED),
            state_notify: Notify::new(),
            error: StdMutex::new(None),
            remote_credits: Arc::new(Semaphore::new(0)),
            peer_recv_capacity: AtomicUsize::new(0),
            credits_in_flight: AtomicUsize::new(0),
            local_recv_capacity: config.recv_count,
            local_send_capacity: config.send_count,
            buffer_size: config.buffer_size,
            pools: OnceLock::new(),
            handshake: StdMutex::new(EngineHandshake {
                hello_send_posted: false,
                hello_send_complete: false,
                hello_receive_complete: false,
            }),
            events: StdMutex::new(VecDeque::new()),
            received: StdMutex::new(VecDeque::new()),
            recv_notify: Notify::new(),
            pending_credit_returns: AtomicUsize::new(0),
            link: OnceLock::new(),
            self_weak: OnceLock::new(),
            #[cfg(any(test, feature = "test-hooks"))]
            hello_override: config.hello_override,
            #[cfg(any(test, feature = "test-hooks"))]
            attach_hook: config.attach_hook.clone(),
        }
    }

    fn install_pools(
        &self,
        data_sends: Vec<Mr>,
        control_sends: Vec<Mr>,
        hello_send: Mr,
    ) -> Result<()> {
        if data_sends.len() != self.local_send_capacity {
            return Err(Error::InvalidConfig(format!(
                "message data-send pool has {} buffers, expected {}",
                data_sends.len(),
                self.local_send_capacity
            )));
        }
        if control_sends.len() != protocol::CTRL_SEND_COUNT {
            return Err(Error::InvalidConfig(format!(
                "message control-send pool has {} buffers, expected {}",
                control_sends.len(),
                protocol::CTRL_SEND_COUNT
            )));
        }
        self.pools
            .set(EngineMessagePools {
                data_sends: RegisteredMrPool::new(data_sends),
                control_sends: RegisteredMrPool::new(control_sends),
                hello_send: StdMutex::new(Some(hello_send)),
            })
            .map_err(|_| Error::InvalidConfig("message pools installed more than once".into()))
    }

    fn pools(&self) -> Result<&EngineMessagePools> {
        self.pools
            .get()
            .ok_or_else(|| Error::InvalidConfig("message pools are not installed".into()))
    }

    fn close_pools(&self) {
        if let Some(pools) = self.pools.get() {
            pools.data_sends.close();
            pools.control_sends.close();
            drop(lock_std(&pools.hello_send).take());
        }
    }

    fn attach(self: &Arc<Self>, connection: &RdmaConnection) -> Result<()> {
        self.pools()?;
        self.self_weak
            .set(Arc::downgrade(self))
            .map_err(|_| Error::InvalidConfig("message state attached more than once".into()))?;
        self.link
            .set(EngineMessageLink {
                shared: Arc::downgrade(&connection.shared),
                connection: Arc::downgrade(&connection.state),
            })
            .map_err(|_| Error::InvalidConfig("message state attached more than once".into()))?;
        connection.attach_ready_work(Arc::clone(self) as Arc<dyn ConnectionReadyWork>)?;
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(hook) = self.attach_hook.as_ref() {
            hook.pause_after_ready_work_attach(self);
        }
        self.enqueue_event(EngineMessageEvent::Start);
        Ok(())
    }

    fn weak_self(&self) -> Weak<Self> {
        self.self_weak.get().cloned().unwrap_or_default()
    }

    fn enqueue_event(&self, event: EngineMessageEvent) {
        let mut event = Some(event);
        {
            let mut events = lock_std(&self.events);
            if self.state.load(Ordering::Acquire) < STATE_CLOSING {
                events.push_back(event.take().expect("event is present"));
            }
        }
        if let Some(event) = event {
            self.dispose_terminal_event(event);
            return;
        }
        self.publish();
    }

    fn dispose_terminal_event(&self, event: EngineMessageEvent) {
        match event {
            EngineMessageEvent::Start => {}
            EngineMessageEvent::HelloSend(completion)
            | EngineMessageEvent::Receive(completion)
            | EngineMessageEvent::ControlSendComplete(completion) => match completion {
                DetachedOperationCompletion::Unaccepted { mr, .. }
                | DetachedOperationCompletion::Completed { mr: Some(mr), .. } => drop(mr),
                DetachedOperationCompletion::Completed { mr: None, .. } => {}
            },
            EngineMessageEvent::Repost(mr) => drop(mr),
            EngineMessageEvent::SendRequest(request) => match request.start() {
                EngineSendRequestAction::Post { mr, credit, .. }
                | EngineSendRequestAction::Cancelled { mr, credit } => {
                    drop(credit);
                    drop(mr);
                    request.complete(Err(self.terminal_error()));
                }
                EngineSendRequestAction::AlreadyHandled => {}
            },
            EngineMessageEvent::SendComplete {
                request,
                completion,
            } => {
                if matches!(&completion, DetachedOperationCompletion::Unaccepted { .. }) {
                    self.rollback_unaccepted_send(&request);
                }
                match completion {
                    DetachedOperationCompletion::Unaccepted { mr, .. }
                    | DetachedOperationCompletion::Completed { mr: Some(mr), .. } => drop(mr),
                    DetachedOperationCompletion::Completed { mr: None, .. } => {}
                }
                request.complete(Err(self.terminal_error()));
            }
        }
    }

    fn drain_terminal_events(&self) {
        let events = std::mem::take(&mut *lock_std(&self.events));
        for event in events {
            self.dispose_terminal_event(event);
        }
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
        // Hold the error mutex across the CAS. A waiter that observes FAILED
        // then blocks on this mutex until the contextual error is published.
        let mut stored = lock_std(&self.error);
        if !transition_terminal(&self.state, STATE_FAILED) {
            return;
        }
        if stored.is_none() {
            *stored = Some(error);
        }
        drop(stored);
        self.remote_credits.close();
        self.pending_credit_returns.store(0, Ordering::Release);
        self.close_pools();
        self.drain_terminal_events();
        self.state_notify.notify_waiters();
        self.recv_notify.notify_waiters();
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
        self.pending_credit_returns.store(0, Ordering::Release);
        self.close_pools();
        self.drain_terminal_events();
        self.state_notify.notify_waiters();
        self.recv_notify.notify_waiters();
        self.publish();
    }

    fn finish_close(&self, result: &Result<()>) {
        match result {
            Ok(()) => {
                transition_terminal(&self.state, STATE_STOPPED);
            }
            Err(error) => self.fail(error.clone(), false),
        }
        self.close_pools();
        self.drain_terminal_events();
        self.state_notify.notify_waiters();
        self.recv_notify.notify_waiters();
    }

    fn process_event(&self, event: EngineMessageEvent) {
        match event {
            EngineMessageEvent::Start => self.start_hello_send(),
            EngineMessageEvent::HelloSend(completion) => match completion {
                DetachedOperationCompletion::Unaccepted { error, mr } => {
                    drop(mr);
                    self.fail(error, true);
                }
                DetachedOperationCompletion::Completed { result, mr } => {
                    drop(mr);
                    match result {
                        Ok(_) => {
                            lock_std(&self.handshake).hello_send_complete = true;
                            self.try_mark_ready();
                        }
                        Err(error) => self.fail(normalize_message_completion_error(error), true),
                    }
                }
            },
            EngineMessageEvent::Receive(completion) => self.process_receive(completion),
            EngineMessageEvent::Repost(mr) => self.process_repost(mr),
            EngineMessageEvent::SendRequest(request) => self.process_send_request(request),
            EngineMessageEvent::SendComplete {
                request,
                completion,
            } => self.process_send_completion(request, completion),
            EngineMessageEvent::ControlSendComplete(completion) => {
                let (result, mr) = match completion {
                    DetachedOperationCompletion::Unaccepted { error, mr } => (Err(error), Some(mr)),
                    DetachedOperationCompletion::Completed { result, mr } => (result, mr),
                };
                let Some(mr) = mr else {
                    self.fail(Error::DriverShutdown, true);
                    return;
                };
                match self.pools() {
                    Ok(pools) => pools.control_sends.put(mr),
                    Err(error) => {
                        drop(mr);
                        self.fail(error, true);
                        return;
                    }
                }
                if let Err(error) = result.map_err(normalize_message_completion_error)
                    && self.state.load(Ordering::Acquire) == STATE_READY
                {
                    // A control-send CapacityExhausted result is deliberately
                    // terminal, unlike queued DATA backpressure. Retrying it
                    // inside the same bounded work budget could repeatedly
                    // claim and restore CREDIT returns without yielding.
                    self.fail(error, true);
                }
            }
        }
    }

    fn process_receive(&self, completion: DetachedOperationCompletion) {
        let (result, mr) = match completion {
            DetachedOperationCompletion::Unaccepted { error, mr } => (Err(error), Some(mr)),
            DetachedOperationCompletion::Completed { result, mr } => (result, mr),
        };
        let state = self.state.load(Ordering::Acquire);
        if state == STATE_CREATED {
            self.process_hello_receive(result, mr);
            return;
        }
        if state != STATE_READY {
            drop(mr);
            return;
        }

        let completion = match result {
            Ok(completion) => completion,
            Err(error) => {
                drop(mr);
                self.fail(normalize_message_completion_error(error), true);
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
                    "receive length {received_len} exceeds MR length {}",
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
        let payload_end = protocol::HEADER_SIZE + header.payload_len as usize;
        match header.frame_type {
            protocol::FRAME_DATA => {
                let payload_len = header.payload_len as usize;
                if payload_len > self.buffer_size {
                    self.fail(
                        Error::ProtocolViolation(format!(
                            "DATA payload {payload_len} exceeds negotiated maximum {}",
                            self.buffer_size
                        )),
                        true,
                    );
                    return;
                }
                lock_std(&self.received).push_back(CompletedRecv {
                    mr,
                    byte_len: payload_len,
                });
                self.recv_notify.notify_one();
            }
            protocol::FRAME_CREDIT => {
                let credit = match protocol::parse_credit(
                    &mr.as_slice()[protocol::HEADER_SIZE..payload_end],
                ) {
                    Ok(credit) => credit,
                    Err(error) => {
                        self.fail(error, true);
                        return;
                    }
                };
                if let Err(error) = self.validate_and_add_credits(credit.credits) {
                    self.fail(error, true);
                    return;
                }
                self.post_receive(mr, false);
            }
            protocol::FRAME_HELLO => self.fail(
                Error::ProtocolViolation(
                    "unexpected HELLO frame during steady-state operation".into(),
                ),
                true,
            ),
            _ => unreachable!("parse_header rejects unknown frame types"),
        }
    }

    fn process_repost(&self, mr: Mr) {
        if self.state.load(Ordering::Acquire) != STATE_READY {
            drop(mr);
            return;
        }
        self.post_receive(mr, true);
    }

    fn post_receive(&self, mr: Mr, return_credit: bool) {
        let connection = match self.connection() {
            Ok(connection) => connection,
            Err(error) => {
                self.fail(error, false);
                return;
            }
        };
        let state = self.weak_self();
        let posted = connection.post_detached_recv(
            mr,
            Box::new(move |completion| {
                if let Some(state) = state.upgrade() {
                    state.enqueue_event(EngineMessageEvent::Receive(completion));
                } else {
                    // Completion callback ownership includes the receive MR.
                    drop(completion);
                }
            }),
        );
        match posted {
            Ok(()) => {
                if return_credit {
                    self.pending_credit_returns.fetch_add(1, Ordering::AcqRel);
                }
            }
            Err(error) if error.potentially_accepted() => {
                self.fail(error.error().clone(), true);
            }
            Err(_) => {}
        }
    }

    fn process_send_request(&self, request: Arc<EngineSendRequest>) {
        match request.start() {
            EngineSendRequestAction::Cancelled { mr, credit } => {
                drop(credit);
                if let Ok(pools) = self.pools() {
                    pools.data_sends.put(mr);
                }
            }
            EngineSendRequestAction::AlreadyHandled => {}
            EngineSendRequestAction::Post {
                mr,
                credit,
                frame_len,
            } => {
                if self.state.load(Ordering::Acquire) != STATE_READY {
                    drop(credit);
                    if let Ok(pools) = self.pools() {
                        pools.data_sends.put(mr);
                    }
                    request.complete(Err(self.terminal_error()));
                    return;
                }
                if let Some(credit) = credit {
                    credit.forget();
                    self.credits_in_flight.fetch_add(1, Ordering::AcqRel);
                    request.credit_committed.store(true, Ordering::Release);
                }
                let connection = match self.connection() {
                    Ok(connection) => connection,
                    Err(error) => {
                        self.rollback_unaccepted_send(&request);
                        if let Ok(pools) = self.pools() {
                            pools.data_sends.put(mr);
                        }
                        request.complete(Err(error));
                        return;
                    }
                };
                let state = self.weak_self();
                let callback_request = Arc::clone(&request);
                if let Err(error) = connection.post_detached_send(
                    mr,
                    frame_len,
                    Box::new(move |completion| {
                        if let Some(state) = state.upgrade() {
                            state.enqueue_event(EngineMessageEvent::SendComplete {
                                request: callback_request,
                                completion,
                            });
                        }
                    }),
                ) && error.potentially_accepted()
                {
                    request.complete(Err(error.error().clone()));
                    self.fail(error.error().clone(), true);
                }
            }
        }
    }

    fn process_send_completion(
        &self,
        request: Arc<EngineSendRequest>,
        completion: DetachedOperationCompletion,
    ) {
        let (result, mr, unaccepted) = match completion {
            DetachedOperationCompletion::Unaccepted { error, mr } => (Err(error), Some(mr), true),
            DetachedOperationCompletion::Completed { result, mr } => (result, mr, false),
        };
        if unaccepted {
            self.rollback_unaccepted_send(&request);
        }
        if let Some(mr) = mr
            && let Ok(pools) = self.pools()
        {
            pools.data_sends.put(mr);
        }
        let result = result
            .map(|_| ())
            .map_err(normalize_message_completion_error);
        request.complete(result.clone());
        if let Err(error) = result
            && self.state.load(Ordering::Acquire) == STATE_READY
            && !matches!(error, Error::CapacityExhausted)
        {
            self.fail(error, true);
        }
    }

    fn rollback_unaccepted_send(&self, request: &EngineSendRequest) {
        if !request.credit_committed.swap(false, Ordering::AcqRel) {
            return;
        }
        if self
            .credits_in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .is_ok()
        {
            self.remote_credits.add_permits(1);
        }
    }

    fn validate_and_add_credits(&self, credits: u32) -> Result<()> {
        let credits = credits as usize;
        check_credit_return(
            credits as u32,
            self.credits_in_flight.load(Ordering::Acquire),
            self.remote_credits.available_permits(),
            self.peer_recv_capacity.load(Ordering::Acquire),
        )?;
        self.credits_in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(credits)
            })
            .map_err(|value| {
                Error::ProtocolViolation(format!(
                    "CREDIT exceeds in-flight sends (atomic): returned={credits} > in_flight={value}"
                ))
            })?;
        self.remote_credits.add_permits(credits);
        Ok(())
    }

    fn flush_one_credit(&self) -> bool {
        if self.pending_credit_returns.load(Ordering::Acquire) == 0
            || self.state.load(Ordering::Acquire) != STATE_READY
        {
            return false;
        }
        let pools = match self.pools() {
            Ok(pools) => pools,
            Err(error) => {
                self.fail(error, true);
                return true;
            }
        };
        let Some(mut mr) = pools.control_sends.try_take() else {
            return false;
        };
        let credits = claim_pending_credit_returns(&self.pending_credit_returns);
        if credits == 0 {
            pools.control_sends.put(mr);
            return false;
        }
        let frame_len = protocol::write_credit_frame(mr.as_mut_slice(), credits as u32);
        let connection = match self.connection() {
            Ok(connection) => connection,
            Err(error) => {
                pools.control_sends.put(mr);
                if self.state.load(Ordering::Acquire) == STATE_READY {
                    self.pending_credit_returns
                        .fetch_add(credits, Ordering::AcqRel);
                }
                self.fail(error, false);
                return true;
            }
        };
        let state = self.weak_self();
        match connection.post_detached_send(
            mr,
            frame_len,
            Box::new(move |completion| {
                if let Some(state) = state.upgrade() {
                    state.enqueue_event(EngineMessageEvent::ControlSendComplete(completion));
                } else {
                    drop(completion);
                }
            }),
        ) {
            Ok(()) => {}
            Err(error) if error.potentially_accepted() => {
                self.fail(error.error().clone(), true);
            }
            Err(_) => {
                if self.state.load(Ordering::Acquire) == STATE_READY {
                    self.pending_credit_returns
                        .fetch_add(credits, Ordering::AcqRel);
                }
            }
        }
        true
    }

    fn enqueue_send_request(&self, request: Arc<EngineSendRequest>) {
        self.enqueue_event(EngineMessageEvent::SendRequest(request));
    }

    async fn recv(self: &Arc<Self>) -> Result<ReceivedMessage> {
        loop {
            let recv_notified = self.recv_notify.notified();
            tokio::pin!(recv_notified);
            recv_notified.as_mut().enable();
            let state_notified = self.state_notify.notified();
            tokio::pin!(state_notified);
            state_notified.as_mut().enable();

            if let Some(completed) = lock_std(&self.received).pop_front() {
                return Ok(ReceivedMessage {
                    mr: Some(completed.mr),
                    byte_len: completed.byte_len,
                    repost: self.weak_self(),
                });
            }
            match self.state.load(Ordering::Acquire) {
                STATE_FAILED => return Err(self.terminal_error()),
                STATE_CLOSING | STATE_STOPPED => return Err(Error::TransportClosed),
                _ => {}
            }
            tokio::select! {
                _ = recv_notified.as_mut() => {}
                _ = state_notified.as_mut() => {}
            }
        }
    }

    async fn wait_terminal(&self) -> Error {
        loop {
            let notified = self.state_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.state.load(Ordering::Acquire) {
                STATE_FAILED => return self.terminal_error(),
                STATE_CLOSING | STATE_STOPPED => return Error::TransportClosed,
                _ => notified.await,
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
        let mut mr = match self.pools() {
            Ok(pools) => match lock_std(&pools.hello_send).take() {
                Some(mr) => mr,
                None => {
                    self.fail(
                        Error::InvalidConfig("HELLO send buffer is unavailable".into()),
                        true,
                    );
                    return;
                }
            },
            Err(error) => {
                self.fail(error, true);
                return;
            }
        };
        let advertised_recv = self.local_recv_capacity as u32;
        let advertised_size = self.buffer_size as u32;
        #[cfg(any(test, feature = "test-hooks"))]
        let (advertised_recv, advertised_size) = {
            let mut advertised_recv = advertised_recv;
            let mut advertised_size = advertised_size;
            match self.hello_override {
                Some(TestHelloOverride::ZeroReceiveCredits) => {
                    advertised_recv = 0;
                }
                Some(TestHelloOverride::SmallerMaximumMessage) => {
                    advertised_size = advertised_size.saturating_sub(1);
                }
                _ => {}
            }
            (advertised_recv, advertised_size)
        };
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
            Box::new(move |completion| {
                if let Some(state) = state.upgrade() {
                    state.enqueue_event(EngineMessageEvent::HelloSend(completion));
                }
            }),
        ) && error.potentially_accepted()
        {
            self.fail(error.error().clone(), true);
        }
    }

    fn process_hello_receive(&self, result: Result<super::op::Completion>, mr: Option<Mr>) {
        let completion = match result {
            Ok(completion) => completion,
            Err(error) => {
                drop(mr);
                self.fail(normalize_message_completion_error(error), true);
                return;
            }
        };
        let Some(mr) = mr else {
            self.fail(Error::DriverShutdown, true);
            return;
        };
        let peer_capacity = match self.parse_hello_receive(&completion, &mr) {
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
        if let Err(error) = connection.post_detached_recv(
            mr,
            Box::new(move |completion| {
                if let Some(state) = state.upgrade() {
                    state.enqueue_event(EngineMessageEvent::Receive(completion));
                } else {
                    // Completion callback ownership includes the receive MR.
                    drop(completion);
                }
            }),
        ) {
            if error.potentially_accepted() {
                self.fail(error.error().clone(), true);
            }
            return;
        }
        self.peer_recv_capacity
            .store(peer_capacity, Ordering::Release);
        self.remote_credits.add_permits(peer_capacity);
        lock_std(&self.handshake).hello_receive_complete = true;
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(hook) = self.attach_hook.as_ref() {
            hook.record_hello_processed();
        }
        self.try_mark_ready();
    }

    fn parse_hello_receive(&self, completion: &super::op::Completion, mr: &Mr) -> Result<usize> {
        let received_len = completion.byte_len() as usize;
        if received_len > mr.len() {
            return Err(Error::ProtocolViolation(format!(
                "HELLO receive length {received_len} exceeds MR length {}",
                mr.len()
            )));
        }
        let header = protocol::parse_header(mr.as_slice(), received_len)?;
        if header.frame_type != protocol::FRAME_HELLO {
            return Err(Error::ProtocolViolation(format!(
                "expected HELLO, got frame_type={}",
                header.frame_type
            )));
        }
        let payload_end = protocol::HEADER_SIZE
            .checked_add(header.payload_len as usize)
            .ok_or_else(|| Error::ProtocolViolation("HELLO payload length overflow".into()))?;
        let hello = protocol::parse_hello(&mr.as_slice()[protocol::HEADER_SIZE..payload_end])?;
        validate_peer_hello(hello, self.buffer_size)
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
            let event = { lock_std(&self.events).pop_front() };
            if let Some(event) = event {
                self.process_event(event);
            } else if !self.flush_one_credit() {
                break;
            }
            processed += 1;
        }
        processed
    }

    fn has_work(&self) -> bool {
        !lock_std(&self.events).is_empty()
            || (self.pending_credit_returns.load(Ordering::Acquire) != 0
                && self
                    .pools
                    .get()
                    .is_some_and(|pools| pools.control_sends.available() != 0))
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

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestEngineHelloDeadlineState {
    state: Arc<EngineMessageState>,
}

#[cfg(test)]
impl TestEngineHelloDeadlineState {
    pub(crate) fn new() -> Self {
        let config = MessageTransportBuilder::new()
            .derive_engine_config()
            .expect("default message config");
        Self {
            state: Arc::new(EngineMessageState::new(&config)),
        }
    }

    pub(crate) fn ready_work(&self) -> Arc<dyn ConnectionReadyWork> {
        Arc::clone(&self.state) as Arc<dyn ConnectionReadyWork>
    }

    pub(crate) async fn ready(&self) -> Result<()> {
        self.state.ready().await
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
/// # Task Count
///
/// Message transport adds zero tasks beyond the engine driver. `send()` and
/// `recv()` run in the caller's task context.
///
/// # Cancellation Safety
///
/// - Cancelling `send()` before WR posting: the credit permit is returned
///   automatically. No resource leak.
/// - Cancelling `send()` after WR posting: the engine retains the MR until
///   the exact CQE arrives.
/// - Cancelling `recv()`: the message stays in the internal channel
///   for the next `recv()` call. No message is lost.
///
/// There is no public error snapshot method. Observe the contextual result of
/// [`Self::ready`], [`Self::send`], [`Self::recv`], and [`Self::close`], plus
/// engine-wide summaries from [`RdmaEngine::diagnostics`].
pub struct MessageTransport {
    buffer_size: usize,
    connection: RdmaConnection,
    state: Arc<EngineMessageState>,
}

impl MessageTransport {
    fn from_engine(
        connection: RdmaConnection,
        state: Arc<EngineMessageState>,
        buffer_size: usize,
    ) -> Self {
        Self {
            buffer_size,
            connection,
            state,
        }
    }

    /// Wait for the transport to become ready (HELLO handshake complete).
    pub async fn ready(&self) -> Result<()> {
        self.state.ready().await
    }

    /// Wait for readiness internally.
    async fn await_ready(&self) -> Result<()> {
        self.ready().await
    }

    /// Send a message. Returns when the local send completion arrives.
    ///
    /// # Errors
    ///
    /// - [`Error::MessageTooLarge`] if `data.len() > buffer_size`
    /// - [`Error::TransportClosed`] if the transport is shut down or disconnected
    /// - [`Error::CompletionError`] if a non-flush send WR completed with error
    /// - [`Error::TransportClosed`] for peer disconnect or ordinary flush teardown
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        if data.len() > self.buffer_size {
            return Err(Error::MessageTooLarge {
                size: data.len(),
                capacity: self.buffer_size,
            });
        }
        self.await_ready().await?;
        let state = Arc::clone(&self.state);
        let acquire_credit = Arc::clone(&state.remote_credits).acquire_owned();
        tokio::pin!(acquire_credit);
        let credit = tokio::select! {
            biased;
            credit = &mut acquire_credit => credit.map_err(|_| state.terminal_error())?,
            error = state.wait_terminal() => return Err(error),
        };
        let pools = state.pools()?;
        let take_mr = pools.data_sends.take();
        tokio::pin!(take_mr);
        let mut mr = tokio::select! {
            biased;
            mr = &mut take_mr => mr.map_err(|_| state.terminal_error())?,
            error = state.wait_terminal() => return Err(error),
        };
        let frame_len = protocol::write_data_frame(mr.as_mut_slice(), data);
        let request = EngineSendRequest::new(mr, Some(credit), frame_len);
        state.enqueue_send_request(Arc::clone(&request));
        let waiter = EngineSendWaiter {
            request,
            state: Arc::downgrade(&state),
            done: false,
        };
        tokio::pin!(waiter);
        tokio::select! {
            biased;
            result = &mut waiter => result,
            error = state.wait_terminal() => Err(error),
        }
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
    /// - The contextual engine or protocol error if the connection failed
    pub async fn recv(&self) -> Result<ReceivedMessage> {
        Arc::clone(&self.state).recv().await
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_connection(&self) -> Result<RdmaConnection> {
        Ok(self.connection.clone())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_pending_ready_work(&self) -> Result<usize> {
        Ok(lock_std(&self.state.events).len())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_available_send_buffers(&self) -> Result<usize> {
        Ok(self.state.pools()?.data_sends.available())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_negotiated_credits(&self) -> Result<(usize, usize)> {
        Ok((
            self.state.remote_credits.available_permits(),
            self.state.credits_in_flight.load(Ordering::Acquire),
        ))
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub async fn test_send_frame(&self, frame: TestSteadyFrame) -> Result<()> {
        self.state.ready().await?;
        let state = Arc::clone(&self.state);
        let pools = state.pools()?;
        let take_mr = pools.data_sends.take();
        tokio::pin!(take_mr);
        let mut mr = tokio::select! {
            biased;
            mr = &mut take_mr => mr.map_err(|_| state.terminal_error())?,
            error = state.wait_terminal() => return Err(error),
        };
        let frame_len = match frame {
            TestSteadyFrame::Credit(credits) => {
                protocol::write_credit_frame(mr.as_mut_slice(), credits)
            }
            TestSteadyFrame::Hello => protocol::write_hello_frame(
                mr.as_mut_slice(),
                state.local_recv_capacity as u32,
                state.buffer_size as u32,
            ),
            TestSteadyFrame::BadMagicData => {
                let len = protocol::write_data_frame(mr.as_mut_slice(), b"x");
                mr.as_mut_slice()[0] ^= 0xff;
                len
            }
            TestSteadyFrame::TrailingDataByte => {
                let len = protocol::write_data_frame(mr.as_mut_slice(), b"");
                mr.as_mut_slice()[len] = 0xff;
                len + 1
            }
            TestSteadyFrame::TruncatedDataPayload => {
                let len = protocol::write_data_frame(mr.as_mut_slice(), b"x");
                mr.as_mut_slice()[8..12].copy_from_slice(&2u32.to_le_bytes());
                len
            }
        };
        let request = EngineSendRequest::new(mr, None, frame_len);
        state.enqueue_send_request(Arc::clone(&request));
        let waiter = EngineSendWaiter {
            request,
            state: Arc::downgrade(&state),
            done: false,
        };
        tokio::pin!(waiter);
        tokio::select! {
            biased;
            result = &mut waiter => result,
            error = state.wait_terminal() => Err(error),
        }
    }

    /// Graceful async shutdown.
    ///
    /// Closes the engine-owned connection and returns its contextual result.
    /// Repeated calls observe the same connection close outcome. A connection
    /// drain timeout returns memoized [`Error::ConnectionQuarantined`]; an
    /// engine-wide terminal drain failure returns [`Error::EngineWedged`].
    pub async fn close(&self) -> Result<()> {
        self.state.begin_close();
        let result = self.connection.close().await;
        self.state.finish_close(&result);
        match result {
            Err(error) => Err(error),
            Ok(()) => lock_std(&self.state.error).clone().map_or(Ok(()), Err),
        }
    }
}

impl Drop for MessageTransport {
    fn drop(&mut self) {
        self.state.begin_close();
    }
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
        assert_eq!(config.connection.max_send_wr, 19);
        assert_eq!(config.connection.max_recv_wr, 34);
        assert_eq!(
            config.connection.max_send_wr + config.connection.max_recv_wr,
            53
        );
        assert_eq!(256 * 53, 13_568);
        assert_eq!(16_384 - 13_568, 2_816);

        let minimum = MessageTransportBuilder::new()
            .send_buffers(1)
            .recv_buffers(1)
            .derive_engine_config()
            .unwrap();
        assert_eq!(minimum.connection.max_send_wr, 4);
        assert_eq!(minimum.connection.max_recv_wr, 3);
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
    use std::sync::Barrier;

    fn hello(capacity: u32, maximum: u32) -> protocol::HelloPayload {
        protocol::HelloPayload {
            data_recv_capacity: capacity,
            max_message_size: maximum,
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

    #[test]
    fn failed_transition_never_overwrites_stopped() {
        let config = MessageTransportBuilder::new()
            .derive_engine_config()
            .unwrap();
        for _ in 0..256 {
            let state = Arc::new(EngineMessageState::new(&config));
            let race = Arc::new(Barrier::new(3));
            let stopped_state = Arc::clone(&state);
            let stopped_race = Arc::clone(&race);
            let stopped = std::thread::spawn(move || {
                stopped_race.wait();
                stopped_state.finish_close(&Ok(()));
            });
            let failed_state = Arc::clone(&state);
            let failed_race = Arc::clone(&race);
            let failed = std::thread::spawn(move || {
                failed_race.wait();
                failed_state.fail(Error::DriverShutdown, false);
            });
            race.wait();
            stopped.join().unwrap();
            failed.join().unwrap();

            match state.state.load(Ordering::Acquire) {
                STATE_STOPPED => assert!(lock_std(&state.error).is_none()),
                STATE_FAILED => assert!(matches!(
                    lock_std(&state.error).as_ref(),
                    Some(Error::DriverShutdown)
                )),
                other => panic!("unexpected terminal state {other}"),
            }
            let terminal = state.state.load(Ordering::Acquire);
            state.fail(Error::ProtocolViolation("late failure".into()), false);
            assert_eq!(state.state.load(Ordering::Acquire), terminal);
        }
    }
}

fn normalize_message_completion_error(error: Error) -> Error {
    match error {
        Error::CompletionError {
            status: crate::wc::WcStatus::WrFlushErr,
            ..
        } => Error::TransportClosed,
        error => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_state() -> Arc<EngineMessageState> {
        let config = MessageTransportBuilder::new()
            .derive_engine_config()
            .expect("default engine message configuration");
        Arc::new(EngineMessageState::new(&config))
    }

    #[test]
    fn engine_credit_returns_are_atomic_capped_and_duplicate_safe() {
        let state = engine_state();
        state.peer_recv_capacity.store(4, Ordering::Release);
        state.remote_credits.add_permits(1);
        state.credits_in_flight.store(3, Ordering::Release);

        state.validate_and_add_credits(2).unwrap();
        assert_eq!(state.remote_credits.available_permits(), 3);
        assert_eq!(state.credits_in_flight.load(Ordering::Acquire), 1);
        assert!(matches!(
            state.validate_and_add_credits(2),
            Err(Error::ProtocolViolation(message)) if message.contains("exceeds in-flight")
        ));
        assert_eq!(state.remote_credits.available_permits(), 3);
        assert_eq!(state.credits_in_flight.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn engine_terminal_failure_wakes_ready_recv_and_send_terminal_waiters() {
        let state = engine_state();
        let ready_state = Arc::clone(&state);
        let ready = tokio::spawn(async move { ready_state.ready().await });
        let recv_state = Arc::clone(&state);
        let recv = tokio::spawn(async move { recv_state.recv().await });
        let terminal_state = Arc::clone(&state);
        let terminal = tokio::spawn(async move { terminal_state.wait_terminal().await });
        tokio::task::yield_now().await;

        state.terminalize(Error::ProtocolViolation("steady-state failure".into()));
        assert!(matches!(
            ready.await.unwrap(),
            Err(Error::ProtocolViolation(message)) if message == "steady-state failure"
        ));
        assert!(matches!(
            recv.await.unwrap(),
            Err(Error::ProtocolViolation(message)) if message == "steady-state failure"
        ));
        assert!(matches!(
            terminal.await.unwrap(),
            Error::ProtocolViolation(message) if message == "steady-state failure"
        ));
    }

    #[test]
    fn disconnected_engine_message_state_is_connection_local_terminal() {
        let state = engine_state();
        state.disconnected();
        assert_eq!(state.state.load(Ordering::Acquire), STATE_FAILED);
        assert!(matches!(state.terminal_error(), Error::TransportClosed));
    }

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
        let err = check_credit_return(0, 4, 0, 4).unwrap_err();
        assert!(matches!(err, Error::ProtocolViolation(_)));
        let err = check_credit_return(10, 0, 4, 4).unwrap_err();
        assert!(matches!(err, Error::ProtocolViolation(_)));
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
        assert!(builder.connection_config.is_none());
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
        let connection = RdmaConnectionConfig::default()
            .max_send_wr(7)
            .max_recv_wr(10);
        let b = MessageTransportBuilder::new()
            .recv_buffers(8)
            .send_buffers(4)
            .buffer_size(4096)
            .connection_config(connection.clone());
        assert_eq!(b.recv_buffer_count, 8);
        assert_eq!(b.send_buffer_count, 4);
        assert_eq!(b.buffer_size, 4096);
        assert_eq!(b.connection_config, Some(connection));
    }

    #[test]
    fn test_derive_config_with_protocol_overhead() {
        let b = MessageTransportBuilder::new()
            .recv_buffers(8)
            .send_buffers(4);
        let cfg = b.derive_engine_config().unwrap();
        // max_send_wr = send_count + ctrl_send_count + 1 = 4 + 2 + 1 = 7
        assert_eq!(cfg.connection.max_send_wr, 7);
        // max_recv_wr = recv_count + ctrl_recv_count = 8 + 2 = 10
        assert_eq!(cfg.connection.max_recv_wr, 10);
    }

    #[test]
    fn pending_credit_claim_is_atomic_and_non_wrapping() {
        let pending = AtomicUsize::new(3);
        assert_eq!(claim_pending_credit_returns(&pending), 3);
        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert_eq!(claim_pending_credit_returns(&pending), 0);
    }

    #[test]
    fn missing_control_send_mr_fails_the_message_connection() {
        let state = engine_state();
        state.state.store(STATE_READY, Ordering::Release);
        state.process_event(EngineMessageEvent::ControlSendComplete(
            DetachedOperationCompletion::Completed {
                result: Err(Error::TransportClosed),
                mr: None,
            },
        ));
        assert_eq!(state.state.load(Ordering::Acquire), STATE_FAILED);
        assert!(matches!(state.terminal_error(), Error::DriverShutdown));
    }

    #[test]
    fn close_and_enqueue_synchronize_state_with_the_event_queue() {
        use std::sync::Barrier;

        for _ in 0..256 {
            let state = engine_state();
            let barrier = Arc::new(Barrier::new(3));
            let close_state = Arc::clone(&state);
            let close_barrier = Arc::clone(&barrier);
            let close = std::thread::spawn(move || {
                close_barrier.wait();
                close_state.begin_close();
            });
            let enqueue_state = Arc::clone(&state);
            let enqueue_barrier = Arc::clone(&barrier);
            let enqueue = std::thread::spawn(move || {
                enqueue_barrier.wait();
                enqueue_state.enqueue_event(EngineMessageEvent::Start);
            });
            barrier.wait();
            close.join().unwrap();
            enqueue.join().unwrap();
            assert!(lock_std(&state.events).is_empty());
        }
    }
}
