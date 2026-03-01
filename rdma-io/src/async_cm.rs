//! Async RDMA Connection Manager — non-blocking connect/accept over rdma_cm.
//!
//! Provides [`AsyncCmId`] for async client connections and [`AsyncCmListener`]
//! for async server accept, using the `rdma_event_channel` fd with tokio's
//! `AsyncFd` for true async I/O (no `spawn_blocking`).
//!
//! These are the building blocks for higher-level types like `AsyncRdmaStream`,
//! but can also be used directly with `AsyncQp` for custom RDMA patterns.

use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;

use tokio::io::unix::AsyncFd;

use crate::Result;
use crate::cm::{CmEventType, CmId, ConnParam, EventChannel, PortSpace};
use crate::cq::CompletionQueue;
use crate::device::Context;
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;

/// Async wrapper around an `EventChannel` for non-blocking CM event delivery.
///
/// Uses tokio's `AsyncFd` to await readiness on the event channel fd,
/// then calls `try_get_event()` for non-blocking event retrieval.
pub(crate) struct AsyncEventChannel {
    async_fd: AsyncFd<RawFd>,
}

impl AsyncEventChannel {
    /// Create a new async event channel wrapper.
    ///
    /// The underlying `EventChannel` must already be set to non-blocking mode.
    pub(crate) fn new(ch: &EventChannel) -> Result<Self> {
        let async_fd = AsyncFd::new(ch.fd()).map_err(crate::Error::Verbs)?;
        Ok(Self { async_fd })
    }

    /// Wait for the next CM event, returning it when available.
    pub(crate) async fn get_event(&self, ch: &EventChannel) -> Result<crate::cm::CmEvent> {
        loop {
            let mut guard = self
                .async_fd
                .readable()
                .await
                .map_err(crate::Error::Verbs)?;
            match ch.try_get_event() {
                Ok(ev) => return Ok(ev),
                Err(crate::Error::WouldBlock) => {
                    guard.clear_ready();
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Wait for a specific CM event type, ack it, and return.
    pub(crate) async fn expect_event(
        &self,
        ch: &EventChannel,
        expected: CmEventType,
    ) -> Result<()> {
        let ev = self.get_event(ch).await?;
        let actual = ev.event_type();
        if actual != expected {
            ev.ack();
            return Err(crate::Error::InvalidArg(format!(
                "expected {expected:?}, got {actual:?}"
            )));
        }
        ev.ack();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AsyncCmId — async client-side CM ID
// ---------------------------------------------------------------------------

/// An async RDMA CM ID for client-side connections.
///
/// Wraps `CmId` + `EventChannel` and provides async versions of the CM
/// operations (`resolve_addr`, `resolve_route`, `connect`). Use with
/// `AsyncQp` for direct async RDMA verb access, or use `AsyncRdmaStream`
/// for a higher-level TCP-like interface.
///
/// # Example
///
/// ```no_run
/// use rdma_io::async_cm::AsyncCmId;
///
/// # async fn example() -> rdma_io::Result<()> {
/// let cm_id = AsyncCmId::connect_to(&"10.0.0.1:9999".parse().unwrap()).await?;
///
/// // Access the inner CmId for QP setup, verb operations, etc.
/// let ctx = cm_id.verbs_context().unwrap();
/// let pd = cm_id.alloc_pd()?;
/// # Ok(())
/// # }
/// ```
pub struct AsyncCmId {
    event_channel: EventChannel,
    cm_id: CmId,
}

// Safety: EventChannel + CmId are Send-safe (raw pointers guarded by kernel).
unsafe impl Send for AsyncCmId {}

impl AsyncCmId {
    /// Create a new async CM ID on its own event channel.
    pub fn new(port_space: PortSpace) -> Result<Self> {
        let ch = EventChannel::new()?;
        ch.set_nonblocking()?;
        let cm_id = CmId::new(&ch, port_space)?;
        Ok(Self {
            event_channel: ch,
            cm_id,
        })
    }

    /// Resolve the destination address asynchronously.
    pub async fn resolve_addr(
        &self,
        src: Option<&SocketAddr>,
        dst: &SocketAddr,
        timeout_ms: i32,
    ) -> Result<()> {
        let async_ch = AsyncEventChannel::new(&self.event_channel)?;
        self.cm_id.resolve_addr(src, dst, timeout_ms)?;
        async_ch
            .expect_event(&self.event_channel, CmEventType::AddrResolved)
            .await
    }

    /// Resolve the route asynchronously.
    pub async fn resolve_route(&self, timeout_ms: i32) -> Result<()> {
        let async_ch = AsyncEventChannel::new(&self.event_channel)?;
        self.cm_id.resolve_route(timeout_ms)?;
        async_ch
            .expect_event(&self.event_channel, CmEventType::RouteResolved)
            .await
    }

    /// Perform the RDMA connect handshake asynchronously.
    pub async fn connect(&self, param: &ConnParam) -> Result<()> {
        let async_ch = AsyncEventChannel::new(&self.event_channel)?;
        self.cm_id.connect(param)?;
        async_ch
            .expect_event(&self.event_channel, CmEventType::Established)
            .await
    }

    /// Full async connect: resolve_addr → resolve_route → connect.
    ///
    /// Convenience method that performs all three CM phases.
    pub async fn connect_to(addr: &SocketAddr) -> Result<Self> {
        let cm = Self::new(PortSpace::Tcp)?;
        cm.resolve_addr(None, addr, 2000).await?;
        cm.resolve_route(2000).await?;
        cm.connect(&ConnParam::default()).await?;
        Ok(cm)
    }

    /// Access the inner `CmId` for QP setup, verb operations, etc.
    pub fn cm_id(&self) -> &CmId {
        &self.cm_id
    }

    /// Access the inner `EventChannel`.
    pub fn event_channel(&self) -> &EventChannel {
        &self.event_channel
    }

    // --- Delegate common CmId methods for convenience ---

    /// Get the verbs context (device) associated with this CM ID.
    pub fn verbs_context(&self) -> Option<Arc<Context>> {
        self.cm_id.verbs_context()
    }

    /// Allocate a protection domain on this CM ID's device.
    pub fn alloc_pd(&self) -> Result<Arc<ProtectionDomain>> {
        self.cm_id.alloc_pd()
    }

    /// Create a QP with separate send/recv CQs on this CM ID.
    pub fn create_qp_with_cq(
        &self,
        pd: &Arc<ProtectionDomain>,
        init_attr: &QpInitAttr,
        send_cq: Option<&Arc<CompletionQueue>>,
        recv_cq: Option<&Arc<CompletionQueue>>,
    ) -> Result<()> {
        self.cm_id
            .create_qp_with_cq(pd, init_attr, send_cq, recv_cq)
    }

    /// Raw QP pointer for use with `AsyncQp`.
    pub fn qp_raw(&self) -> *mut rdma_io_sys::ibverbs::ibv_qp {
        self.cm_id.qp_raw()
    }

    /// Disconnect the connection (fire-and-forget, synchronous).
    pub fn disconnect(&self) -> Result<()> {
        self.cm_id.disconnect()
    }

    /// Disconnect and await the `DISCONNECTED` event from the peer.
    ///
    /// This performs a graceful disconnect: sends the disconnect request,
    /// then waits for the peer to acknowledge it. Analogous to TCP's
    /// `shutdown()` + await FIN-ACK.
    pub async fn disconnect_async(&self) -> Result<()> {
        let async_ch = AsyncEventChannel::new(&self.event_channel)?;
        self.cm_id.disconnect()?;
        async_ch
            .expect_event(&self.event_channel, CmEventType::Disconnected)
            .await
    }

    /// Await the next CM event on this connection's event channel.
    ///
    /// Returns any event (disconnect, error, etc.). The caller must ack the
    /// event via [`CmEvent::ack()`]. Useful for monitoring connection
    /// lifecycle (e.g., detecting peer disconnect via `select!`).
    pub async fn next_event(&self) -> Result<crate::cm::CmEvent> {
        let async_ch = AsyncEventChannel::new(&self.event_channel)?;
        async_ch.get_event(&self.event_channel).await
    }

    /// Decompose into the inner `EventChannel` and `CmId`.
    ///
    /// Used when transferring ownership to a higher-level type (e.g., `AsyncRdmaStream`).
    pub fn into_parts(self) -> (EventChannel, CmId) {
        // Prevent Drop from running (it would disconnect)
        let event_channel = unsafe { std::ptr::read(&self.event_channel) };
        let cm_id = unsafe { std::ptr::read(&self.cm_id) };
        std::mem::forget(self);
        (event_channel, cm_id)
    }
}

// ---------------------------------------------------------------------------
// AsyncCmListener — async server-side listener
// ---------------------------------------------------------------------------

/// An async RDMA CM listener for accepting incoming connections.
///
/// Binds to a local address and provides an async `accept()` that returns
/// an [`AsyncCmId`] for each incoming connection. The accepted ID is fully
/// connected and migrated to its own event channel.
///
/// # Example
///
/// ```no_run
/// use rdma_io::async_cm::AsyncCmListener;
///
/// # async fn example() -> rdma_io::Result<()> {
/// let listener = AsyncCmListener::bind(&"0.0.0.0:9999".parse().unwrap())?;
/// let cm_id = listener.accept().await?;
///
/// // Set up QP and start using AsyncQp
/// let ctx = cm_id.verbs_context().unwrap();
/// # Ok(())
/// # }
/// ```
pub struct AsyncCmListener {
    event_channel: EventChannel,
    async_ch: AsyncEventChannel,
    _cm_id: CmId,
}

// Safety: EventChannel + CmId are Send-safe (raw pointers guarded by kernel).
unsafe impl Send for AsyncCmListener {}

impl AsyncCmListener {
    /// Bind to a local address and start listening.
    pub fn bind(addr: &SocketAddr) -> Result<Self> {
        Self::bind_with_backlog(addr, 128)
    }

    /// Bind with a custom backlog.
    pub fn bind_with_backlog(addr: &SocketAddr, backlog: i32) -> Result<Self> {
        let ch = EventChannel::new()?;
        ch.set_nonblocking()?;
        let async_ch = AsyncEventChannel::new(&ch)?;
        let cm_id = CmId::new(&ch, PortSpace::Tcp)?;
        cm_id.listen(addr, backlog)?;
        Ok(Self {
            event_channel: ch,
            async_ch,
            _cm_id: cm_id,
        })
    }

    /// Accept an incoming connection asynchronously.
    ///
    /// Returns an [`AsyncCmId`] that is fully connected and migrated to its
    /// own event channel. The caller can then set up QP resources and use
    /// `AsyncQp` for data transfer.
    ///
    /// Note: QP and CQ must be set up by the caller BEFORE calling this,
    /// or use the lower-level `accept_raw()` for more control.
    pub async fn accept(&self) -> Result<AsyncCmId> {
        self.accept_with_param(&ConnParam::default()).await
    }

    /// Accept with custom connection parameters.
    ///
    /// The accepted connection goes through: await CONNECT_REQUEST →
    /// accept handshake → await ESTABLISHED → migrate to own event channel.
    ///
    /// **Important**: The caller must set up QP resources (PD, CQ, QP, MRs)
    /// on the returned `AsyncCmId` before the connection can transfer data.
    /// For a batteries-included experience, use `AsyncRdmaStream` instead.
    pub async fn accept_with_param(&self, param: &ConnParam) -> Result<AsyncCmId> {
        let conn_id = self.get_request().await?;

        // Accept the connection (non-blocking kernel call)
        conn_id.accept(param)?;

        // Await ESTABLISHED on the listener's event channel
        self.async_ch
            .expect_event(&self.event_channel, CmEventType::Established)
            .await?;

        // Migrate to its own event channel
        let conn_ch = EventChannel::new()?;
        conn_ch.set_nonblocking()?;
        conn_id.migrate(&conn_ch)?;

        Ok(AsyncCmId {
            event_channel: conn_ch,
            cm_id: conn_id,
        })
    }

    /// Await the next CONNECT_REQUEST and return the raw `CmId`.
    ///
    /// This is the first phase of a two-phase accept. The caller can set up
    /// QP resources on the returned `CmId`, then call
    /// [`complete_accept`](Self::complete_accept) to finish the handshake.
    pub async fn get_request(&self) -> Result<CmId> {
        let ev = self.async_ch.get_event(&self.event_channel).await?;
        let etype = ev.event_type();
        if etype != CmEventType::ConnectRequest {
            ev.ack();
            return Err(crate::Error::InvalidArg(format!(
                "expected ConnectRequest, got {etype:?}"
            )));
        }
        let conn_id = unsafe { CmId::from_raw(ev.cm_id_raw(), true) };
        ev.ack();
        Ok(conn_id)
    }

    /// Complete the accept handshake after QP setup.
    ///
    /// Second phase of a two-phase accept: sends the accept reply, awaits
    /// ESTABLISHED, and migrates the connection to its own event channel.
    pub async fn complete_accept(&self, conn_id: CmId, param: &ConnParam) -> Result<AsyncCmId> {
        conn_id.accept(param)?;

        self.async_ch
            .expect_event(&self.event_channel, CmEventType::Established)
            .await?;

        let conn_ch = EventChannel::new()?;
        conn_ch.set_nonblocking()?;
        conn_id.migrate(&conn_ch)?;

        Ok(AsyncCmId {
            event_channel: conn_ch,
            cm_id: conn_id,
        })
    }

    /// Await the next CM event on the listener's event channel.
    ///
    /// Returns any event (connect request, disconnect, error, etc.).
    /// The caller must ack the event via [`CmEvent::ack()`].
    /// For simple accept loops, prefer [`accept()`](Self::accept) instead.
    pub async fn next_event(&self) -> Result<crate::cm::CmEvent> {
        self.async_ch.get_event(&self.event_channel).await
    }
}
