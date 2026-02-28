//! RDMA Connection Manager (rdma_cm).
//!
//! Provides TCP-like connection semantics over RDMA. This is the primary
//! way to use iWARP (siw) and the recommended approach for RoCE.

use std::net::SocketAddr;
use std::sync::Arc;

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::rdmacm::*;

use crate::Result;
use crate::device::Context;
use crate::error::{from_ptr, from_ret_errno};
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;

/// Port space for CM connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSpace {
    Tcp,
    Udp,
    Ib,
    Ipoib,
}

impl PortSpace {
    fn as_raw(self) -> u32 {
        match self {
            Self::Tcp => RDMA_PS_TCP,
            Self::Udp => RDMA_PS_UDP,
            Self::Ib => RDMA_PS_IB,
            Self::Ipoib => RDMA_PS_IPOIB,
        }
    }
}

/// CM event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmEventType {
    AddrResolved,
    AddrError,
    RouteResolved,
    RouteError,
    ConnectRequest,
    ConnectResponse,
    ConnectError,
    Unreachable,
    Rejected,
    Established,
    Disconnected,
    DeviceRemoval,
    MulticastJoin,
    MulticastError,
    AddrChange,
    TimewaitExit,
    Unknown(u32),
}

impl CmEventType {
    fn from_raw(v: u32) -> Self {
        match v {
            RDMA_CM_EVENT_ADDR_RESOLVED => Self::AddrResolved,
            RDMA_CM_EVENT_ADDR_ERROR => Self::AddrError,
            RDMA_CM_EVENT_ROUTE_RESOLVED => Self::RouteResolved,
            RDMA_CM_EVENT_ROUTE_ERROR => Self::RouteError,
            RDMA_CM_EVENT_CONNECT_REQUEST => Self::ConnectRequest,
            RDMA_CM_EVENT_CONNECT_RESPONSE => Self::ConnectResponse,
            RDMA_CM_EVENT_CONNECT_ERROR => Self::ConnectError,
            RDMA_CM_EVENT_UNREACHABLE => Self::Unreachable,
            RDMA_CM_EVENT_REJECTED => Self::Rejected,
            RDMA_CM_EVENT_ESTABLISHED => Self::Established,
            RDMA_CM_EVENT_DISCONNECTED => Self::Disconnected,
            RDMA_CM_EVENT_DEVICE_REMOVAL => Self::DeviceRemoval,
            RDMA_CM_EVENT_MULTICAST_JOIN => Self::MulticastJoin,
            RDMA_CM_EVENT_MULTICAST_ERROR => Self::MulticastError,
            RDMA_CM_EVENT_ADDR_CHANGE => Self::AddrChange,
            RDMA_CM_EVENT_TIMEWAIT_EXIT => Self::TimewaitExit,
            other => Self::Unknown(other),
        }
    }
}

/// Connection parameters for `connect` / `accept`.
#[derive(Debug, Clone)]
pub struct ConnParam {
    /// Responder resources (max incoming RDMA read/atomic).
    pub responder_resources: u8,
    /// Initiator depth (max outstanding RDMA read/atomic).
    pub initiator_depth: u8,
    /// Retry count.
    pub retry_count: u8,
    /// RNR retry count (7 = infinite).
    pub rnr_retry_count: u8,
}

impl Default for ConnParam {
    fn default() -> Self {
        Self {
            responder_resources: 1,
            initiator_depth: 1,
            retry_count: 7,
            rnr_retry_count: 7,
        }
    }
}

impl ConnParam {
    fn to_raw(&self) -> rdma_conn_param {
        rdma_conn_param {
            responder_resources: self.responder_resources,
            initiator_depth: self.initiator_depth,
            retry_count: self.retry_count,
            rnr_retry_count: self.rnr_retry_count,
            ..Default::default()
        }
    }
}

/// An rdma_cm event channel.
///
/// Used to receive CM events (address resolved, connected, etc.).
pub struct EventChannel {
    inner: *mut rdma_event_channel,
}

// Safety: The event channel fd is process-global; get_event serialized by caller.
unsafe impl Send for EventChannel {}
unsafe impl Sync for EventChannel {}

impl Drop for EventChannel {
    fn drop(&mut self) {
        unsafe { rdma_destroy_event_channel(self.inner) };
    }
}

impl EventChannel {
    /// Create a new event channel.
    pub fn new() -> Result<Self> {
        let ch = from_ptr(unsafe { rdma_create_event_channel() })?;
        Ok(Self { inner: ch })
    }

    /// Block until the next CM event arrives.
    pub fn get_event(&self) -> Result<CmEvent> {
        let mut event: *mut rdma_cm_event = std::ptr::null_mut();
        from_ret_errno(unsafe { rdma_get_cm_event(self.inner, &mut event) })?;
        Ok(CmEvent { inner: event })
    }

    /// Raw pointer.
    pub fn as_raw(&self) -> *mut rdma_event_channel {
        self.inner
    }
}

/// A CM event received from an [`EventChannel`].
///
/// **Must be acknowledged** via [`ack`](CmEvent::ack) (consumed on ack).
pub struct CmEvent {
    inner: *mut rdma_cm_event,
}

// Safety: CM events are tied to a channel; single-threaded access pattern.
unsafe impl Send for CmEvent {}

impl CmEvent {
    /// The event type.
    pub fn event_type(&self) -> CmEventType {
        CmEventType::from_raw(unsafe { (*self.inner).event })
    }

    /// Status code (0 = success for most events).
    pub fn status(&self) -> i32 {
        unsafe { (*self.inner).status }
    }

    /// For `ConnectRequest` events, the CM ID of the new incoming connection.
    ///
    /// The returned `CmId` is **not** owned — it must be accepted or rejected.
    /// The caller takes ownership after `accept`.
    pub fn cm_id_raw(&self) -> *mut rdma_cm_id {
        unsafe { (*self.inner).id }
    }

    /// Acknowledge and consume this event. Must be called for every event.
    pub fn ack(self) {
        unsafe { rdma_ack_cm_event(self.inner) };
        std::mem::forget(self); // prevent double-free in Drop
    }
}

impl Drop for CmEvent {
    fn drop(&mut self) {
        // If not ack'd, ack now to avoid leaking the event.
        unsafe { rdma_ack_cm_event(self.inner) };
    }
}

/// An RDMA CM identifier — the core handle for connection management.
///
/// Wraps `rdma_cm_id*`. Use for both client (active) and server (passive) sides.
pub struct CmId {
    pub(crate) inner: *mut rdma_cm_id,
    /// Whether this CmId owns the underlying rdma_cm_id (should call rdma_destroy_id).
    owned: bool,
}

// Safety: rdma_cm_id operations are serialized by the caller.
unsafe impl Send for CmId {}
unsafe impl Sync for CmId {}

impl Drop for CmId {
    fn drop(&mut self) {
        if self.owned {
            unsafe { rdma_destroy_id(self.inner) };
        }
    }
}

impl CmId {
    /// Create a new CM ID on the given event channel.
    pub fn new(channel: &EventChannel, port_space: PortSpace) -> Result<Self> {
        let mut id: *mut rdma_cm_id = std::ptr::null_mut();
        from_ret_errno(unsafe {
            rdma_create_id(
                channel.inner,
                &mut id,
                std::ptr::null_mut(),
                port_space.as_raw(),
            )
        })?;
        Ok(Self {
            inner: id,
            owned: true,
        })
    }

    /// Wrap a raw `rdma_cm_id` pointer (e.g. from a connect request event).
    ///
    /// # Safety
    /// The caller must ensure the pointer is valid and that ownership semantics
    /// are correctly handled.
    pub unsafe fn from_raw(id: *mut rdma_cm_id, owned: bool) -> Self {
        Self { inner: id, owned }
    }

    /// Resolve the destination address.
    pub fn resolve_addr(
        &self,
        src: Option<&SocketAddr>,
        dst: &SocketAddr,
        timeout_ms: i32,
    ) -> Result<()> {
        let (src_ptr, dst_sa) = sockaddr_args(src, dst);
        from_ret_errno(unsafe {
            rdma_resolve_addr(self.inner, src_ptr, dst_sa.as_ptr() as *mut _, timeout_ms)
        })
    }

    /// Resolve the route to the destination.
    pub fn resolve_route(&self, timeout_ms: i32) -> Result<()> {
        from_ret_errno(unsafe { rdma_resolve_route(self.inner, timeout_ms) })
    }

    /// Bind to a local address and start listening.
    pub fn listen(&self, addr: &SocketAddr, backlog: i32) -> Result<()> {
        let sa = to_sockaddr_storage(addr);
        from_ret_errno(unsafe { rdma_bind_addr(self.inner, sa.as_ptr() as *mut _) })?;
        from_ret_errno(unsafe { rdma_listen(self.inner, backlog) })
    }

    /// Create a QP on this CM ID.
    ///
    /// The QP is managed by rdma_cm and destroyed when the CM ID is destroyed.
    pub fn create_qp(&self, pd: &Arc<ProtectionDomain>, init_attr: &QpInitAttr) -> Result<()> {
        let mut raw_attr = ibv_qp_init_attr {
            cap: ibv_qp_cap {
                max_send_wr: init_attr.max_send_wr,
                max_recv_wr: init_attr.max_recv_wr,
                max_send_sge: init_attr.max_send_sge,
                max_recv_sge: init_attr.max_recv_sge,
                max_inline_data: init_attr.max_inline_data,
            },
            qp_type: init_attr.qp_type.as_raw(),
            sq_sig_all: i32::from(init_attr.sq_sig_all),
            ..Default::default()
        };
        from_ret_errno(unsafe { rdma_create_qp(self.inner, pd.inner, &mut raw_attr) })
    }

    /// Connect to a remote peer (client side).
    pub fn connect(&self, param: &ConnParam) -> Result<()> {
        let mut raw = param.to_raw();
        from_ret_errno(unsafe { rdma_connect(self.inner, &mut raw) })
    }

    /// Accept an incoming connection (server side).
    pub fn accept(&self, param: &ConnParam) -> Result<()> {
        let mut raw = param.to_raw();
        from_ret_errno(unsafe { rdma_accept(self.inner, &mut raw) })
    }

    /// Disconnect from the remote peer.
    pub fn disconnect(&self) -> Result<()> {
        from_ret_errno(unsafe { rdma_disconnect(self.inner) })
    }

    /// Get the QP number (if a QP has been created on this CM ID).
    pub fn qp_num(&self) -> Option<u32> {
        let qp = unsafe { (*self.inner).qp };
        if qp.is_null() {
            None
        } else {
            Some(unsafe { (*qp).qp_num })
        }
    }

    /// Get the raw QP pointer (if a QP has been created on this CM ID).
    pub fn qp_raw(&self) -> *mut ibv_qp {
        unsafe { (*self.inner).qp }
    }

    /// Get the ibverbs context associated with this CM ID (set after addr resolution).
    ///
    /// Returns `None` if the address hasn't been resolved yet.
    pub fn verbs_context(&self) -> Option<Arc<Context>> {
        let ctx = unsafe { (*self.inner).verbs };
        if ctx.is_null() {
            None
        } else {
            // rdma_cm owns this context — don't close on drop
            Some(Arc::new(unsafe { Context::from_raw(ctx, false) }))
        }
    }

    /// Allocate a PD from this CM ID's verbs context.
    ///
    /// Convenience for `ProtectionDomain::new(cm_id.verbs_context())`.
    pub fn alloc_pd(&self) -> Result<Arc<ProtectionDomain>> {
        let ctx = self.verbs_context().ok_or(crate::Error::InvalidArg(
            "CM ID has no verbs context (resolve_addr first)".into(),
        ))?;
        ProtectionDomain::new(ctx)
    }

    /// Raw pointer.
    pub fn as_raw(&self) -> *mut rdma_cm_id {
        self.inner
    }
}

// --- Socket address helpers ---

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// Convert a `SocketAddr` to a `sockaddr_storage`-sized buffer.
fn to_sockaddr_storage(addr: &SocketAddr) -> SockAddrBuf {
    let mut buf = [0u8; std::mem::size_of::<bnd_posix::posix::socket::sockaddr_storage>()];
    match addr {
        SocketAddr::V4(v4) => {
            let sa = bnd_posix::posix::inet::sockaddr_in {
                sin_family: AF_INET,
                sin_port: v4.port().to_be(),
                sin_addr: bnd_posix::posix::inet::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                ..Default::default()
            };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sa as *const _ as *const u8,
                    buf.as_mut_ptr(),
                    std::mem::size_of_val(&sa),
                );
            }
        }
        SocketAddr::V6(v6) => {
            let sa = bnd_posix::posix::inet::sockaddr_in6 {
                sin6_family: AF_INET6,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: bnd_posix::posix::inet::in6_addr {
                    __in6_u: bnd_posix::posix::inet::in6_addr___in6_u {
                        __u6_addr8: v6.ip().octets(),
                    },
                },
                sin6_scope_id: v6.scope_id(),
            };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sa as *const _ as *const u8,
                    buf.as_mut_ptr(),
                    std::mem::size_of_val(&sa),
                );
            }
        }
    }
    SockAddrBuf(buf)
}

/// Stack-allocated sockaddr buffer.
struct SockAddrBuf([u8; std::mem::size_of::<bnd_posix::posix::socket::sockaddr_storage>()]);

impl SockAddrBuf {
    fn as_ptr(&self) -> *const bnd_posix::posix::socket::sockaddr {
        self.0.as_ptr().cast()
    }
}

fn sockaddr_args(
    src: Option<&SocketAddr>,
    dst: &SocketAddr,
) -> (*mut bnd_posix::posix::socket::sockaddr, SockAddrBuf) {
    let dst_sa = to_sockaddr_storage(dst);
    let src_ptr = match src {
        // For simplicity, pass null for src (let the kernel choose).
        Some(_) => std::ptr::null_mut(),
        None => std::ptr::null_mut(),
    };
    (src_ptr, dst_sa)
}
