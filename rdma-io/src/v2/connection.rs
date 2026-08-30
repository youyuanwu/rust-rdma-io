//! Ergonomic v2 connection builder for RDMA transport setup.
//!
//! Provides `ConnectionParts` and [`CompletionMode`] for establishing RDMA
//! connections with automatic resource wiring. CQ drivers are created but
//! NOT spawned — the caller composes them into a driver future.

use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed CQ driver future — ready to be polled but NOT spawned.
pub(crate) type BoxedCqDriverFuture =
    Pin<Box<dyn Future<Output = super::error::Result<()>> + Send>>;

use tokio::io::unix::AsyncFd;

use crate::async_cm::{AsyncCmId, AsyncCmListener};
use crate::cm::{ConnParam, EventChannel};

use super::cq::CqBuilder;
use super::driver::{CqDriverHandle, FdCqDriver, PollingCqDriver};
use super::error::{Error, Result};
use super::pd::Pd;
use super::qp::Qp;
use super::shared_qp::SharedQp;

/// CQ completion integration mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionMode {
    /// Fd/readiness-based: lower CPU, slightly higher latency.
    #[default]
    Readiness,
    /// Direct CQ polling: higher CPU, lower latency.
    Polling,
}

/// Internal connection configuration.
pub(crate) struct ConnectionConfig {
    pub(crate) completion_mode: CompletionMode,
    pub(crate) cq_depth: usize,
    pub(crate) max_send_wr: usize,
    pub(crate) max_recv_wr: usize,
    pub(crate) inflight_capacity: usize,
    pub(crate) conn_param: ConnParam,
    pub(crate) separate_cqs: bool,
}

impl ConnectionConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.cq_depth == 0 {
            return Err(Error::InvalidConfig("CQ depth must be > 0".into()));
        }
        if self.max_send_wr == 0 {
            return Err(Error::InvalidConfig("max_send_wr must be > 0".into()));
        }
        if self.max_recv_wr == 0 {
            return Err(Error::InvalidConfig("max_recv_wr must be > 0".into()));
        }
        if self.inflight_capacity == 0 {
            return Err(Error::InvalidConfig("inflight_capacity must be > 0".into()));
        }
        if !self.separate_cqs && self.inflight_capacity < self.max_send_wr + self.max_recv_wr {
            return Err(Error::InvalidConfig(format!(
                "shared CQ inflight_capacity ({}) must be >= max_send_wr ({}) + max_recv_wr ({})",
                self.inflight_capacity, self.max_send_wr, self.max_recv_wr
            )));
        }
        Ok(())
    }
}

/// Handle for the disconnect monitor — carries the `AsyncFd` and a
/// shared reference to the `EventChannel` for reading CM events.
///
/// Does NOT own the `CmId` — that stays in [`ConnectionResources`] where
/// field-declaration-order drop guarantees safe cleanup.
pub(crate) struct CmMonitorHandle {
    pub(crate) cm_async_fd: AsyncFd<RawFd>,
    pub(crate) event_channel: Arc<EventChannel>,
}

/// Resources that must be kept alive for the RDMA connection.
/// Owned by the driver side.
///
/// # Drop Ordering — Known Limitation
///
/// The `CmQueuePair` destructor calls `rdma_destroy_qp(cm_id_raw)`,
/// which requires the creating `CmId` to still be alive. However,
/// `Arc<SharedQp>` may outlive `ConnectionResources` (other references
/// are held via `TransportSharedState`). The `shared_qp` field here
/// is cleared by the driver before returning, but other `Arc` clones
/// may still exist, so the QP may actually be destroyed after the
/// `CmId`. This is a pre-existing issue inherited from the v2 CM
/// architecture and is not specific to the explicit-spawn refactoring.
///
/// Fields drop in declaration order when the struct is dropped.
pub(crate) struct ConnectionResources {
    /// SharedQp must drop first — QP must be destroyed before CmId.
    /// The driver also holds an Arc<SharedQp> for operations; this
    /// owned copy ensures the last Arc is dropped here in correct order.
    pub(crate) shared_qp: Option<Arc<SharedQp>>,
    #[expect(dead_code)] // held for drop ordering / MR lifetime
    pub(crate) pd: Pd,
    #[expect(dead_code)] // held for drop ordering — must drop AFTER shared_qp
    pub(crate) cm_id: crate::cm::CmId,
    #[expect(dead_code)] // held for drop ordering — closes fd after cm_id
    pub(crate) event_channel: Arc<EventChannel>,
}

/// The result of establishing an RDMA connection — all resources needed
/// to construct a frontend transport and a driver future.
///
/// CQ drivers are created but NOT spawned. The caller composes them
/// into a single driver future for explicit user spawning.
pub(crate) struct ConnectionParts {
    pub(crate) shared_qp: SharedQp,
    pub(crate) driver_handles: Vec<Arc<CqDriverHandle>>,
    /// Boxed CQ driver futures ready to be polled. NOT spawned.
    pub(crate) driver_futures: Vec<BoxedCqDriverFuture>,
    pub(crate) resources: ConnectionResources,
    pub(crate) cm_monitor_handle: Option<CmMonitorHandle>,
}

/// An established RDMA connection with all resources wired.
///
/// This type is kept for backward compatibility with lower-level v2 APIs.
/// For message transport, `ConnectionParts` is used internally.
///
/// # Deprecation Notice
///
/// This type is no longer constructible through the public API since the
/// explicit driver spawning refactoring. Use [`super::message_transport::MessageTransportBuilder`]
/// for message transport connections.
#[deprecated(note = "Use MessageTransportBuilder for message transport connections")]
#[allow(deprecated)]
pub struct Connection {
    shared_qp: SharedQp,
    driver_handles: Vec<Arc<CqDriverHandle>>,
    pd: Pd,
    #[expect(dead_code)] // held for drop ordering — must drop AFTER shared_qp
    cm_id: crate::cm::CmId,
    #[expect(dead_code)] // held for drop ordering
    event_channel: Arc<EventChannel>,
    #[expect(dead_code)] // held for drop ordering
    cm_async_fd: Option<AsyncFd<RawFd>>,
    shutdown_initiated: bool,
}

#[allow(deprecated)]
impl Connection {
    /// Access the shared queue pair.
    pub fn shared_qp(&self) -> &SharedQp {
        &self.shared_qp
    }

    /// Access the protection domain.
    pub fn pd(&self) -> &Pd {
        &self.pd
    }

    /// Access the driver handles.
    pub fn driver_handles(&self) -> &[Arc<CqDriverHandle>] {
        &self.driver_handles
    }

    /// Take the CM monitoring handle for a disconnect monitor task.
    #[expect(dead_code)]
    pub(crate) fn take_cm_monitor_handle(&mut self) -> Option<CmMonitorHandle> {
        let async_fd = self.cm_async_fd.take()?;
        Some(CmMonitorHandle {
            cm_async_fd: async_fd,
            event_channel: Arc::clone(&self.event_channel),
        })
    }

    /// Synchronous, idempotent shutdown initiation. Safe for `Drop`.
    pub fn initiate_shutdown(&mut self) {
        if self.shutdown_initiated {
            return;
        }
        self.shutdown_initiated = true;
        let _ = self.shared_qp.shutdown();
        for handle in &self.driver_handles {
            handle.flush_and_shutdown();
        }
    }
}

#[allow(deprecated)]
impl Drop for Connection {
    fn drop(&mut self) {
        self.initiate_shutdown();
    }
}

/// Build and establish an RDMA connection.
pub(crate) struct ConnectionBuilder {
    config: ConnectionConfig,
}

impl ConnectionBuilder {
    pub(crate) fn new(config: ConnectionConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    /// Connect to a remote endpoint (client side).
    ///
    /// `pre_establish` is called after QP creation but before the CM
    /// handshake, allowing the caller to post receive buffers.
    ///
    /// Returns [`ConnectionParts`] with CQ drivers NOT spawned.
    pub(crate) async fn connect<F>(
        self,
        addr: &SocketAddr,
        pre_establish: F,
    ) -> Result<ConnectionParts>
    where
        F: FnOnce(&SharedQp, &Pd) -> Result<()>,
    {
        let async_cm = AsyncCmId::new(crate::cm::PortSpace::Tcp)?;
        async_cm.resolve_addr(None, addr, 2000).await?;
        async_cm.resolve_route(2000).await?;

        let ctx_arc = async_cm
            .verbs_context()
            .ok_or(Error::InvalidConfig("no verbs context".into()))?;
        let ctx = super::context::Context::from_inner(ctx_arc);
        let pd_inner = async_cm.alloc_pd()?;
        let pd = Pd::new(pd_inner);

        // Build CQ(s) and create (but do NOT spawn) drivers
        let depth = self.config.cq_depth as i32;
        let (shared_qp, driver_handles, driver_futures) = if self.config.separate_cqs {
            let (send_cq, recv_cq) = match self.config.completion_mode {
                CompletionMode::Readiness => {
                    let sc = CqBuilder::new(&ctx, depth).with_channel().build()?;
                    let rc = CqBuilder::new(&ctx, depth).with_channel().build()?;
                    (sc, rc)
                }
                CompletionMode::Polling => {
                    let sc = CqBuilder::new(&ctx, depth).build()?;
                    let rc = CqBuilder::new(&ctx, depth).build()?;
                    (sc, rc)
                }
            };

            let cmqp = async_cm.create_qp_with_cq(
                pd.inner(),
                &self.qp_init_attr(),
                Some(send_cq.inner()),
                Some(recv_cq.inner()),
            )?;
            let qp = Qp::from_cm_qp(cmqp);

            let cap = self.config.inflight_capacity;
            let (send_handle, send_future) = self.create_driver(send_cq, cap);
            let (recv_handle, recv_future) = self.create_driver(recv_cq, cap);

            let sqp = SharedQp::new(
                qp,
                Arc::clone(&send_handle),
                Arc::clone(&recv_handle),
                pd.clone(),
            );
            (
                sqp,
                vec![send_handle, recv_handle],
                vec![send_future, recv_future],
            )
        } else {
            let cq = match self.config.completion_mode {
                CompletionMode::Readiness => CqBuilder::new(&ctx, depth).with_channel().build()?,
                CompletionMode::Polling => CqBuilder::new(&ctx, depth).build()?,
            };

            let cmqp = async_cm.create_qp_with_cq(
                pd.inner(),
                &self.qp_init_attr(),
                Some(cq.inner()),
                Some(cq.inner()),
            )?;
            let qp = Qp::from_cm_qp(cmqp);

            let cap = self.config.inflight_capacity;
            let (handle, future) = self.create_driver(cq, cap);

            let sqp = SharedQp::new(qp, Arc::clone(&handle), Arc::clone(&handle), pd.clone());
            (sqp, vec![handle], vec![future])
        };

        // Pre-establish hook
        pre_establish(&shared_qp, &pd)?;

        // Complete CM handshake
        async_cm.connect(&self.config.conn_param).await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let event_channel = Arc::new(event_channel);
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(Error::Verbs)?;

        Ok(ConnectionParts {
            shared_qp,
            driver_handles,
            driver_futures,
            resources: ConnectionResources {
                shared_qp: None, // populated by message_transport after Arc wrapping
                pd,
                cm_id,
                event_channel: Arc::clone(&event_channel),
            },
            cm_monitor_handle: Some(CmMonitorHandle {
                cm_async_fd,
                event_channel,
            }),
        })
    }

    /// Accept a connection (server side).
    ///
    /// Returns [`ConnectionParts`] with CQ drivers NOT spawned.
    pub(crate) async fn accept<F>(
        self,
        listener: &AsyncCmListener,
        pre_establish: F,
    ) -> Result<ConnectionParts>
    where
        F: FnOnce(&SharedQp, &Pd) -> Result<()>,
    {
        let conn_id = listener.get_request().await?;

        let ctx_arc = conn_id
            .verbs_context()
            .ok_or(Error::InvalidConfig("no verbs context".into()))?;
        let ctx = super::context::Context::from_inner(ctx_arc);
        let pd_inner = conn_id.alloc_pd()?;
        let pd = Pd::new(pd_inner);

        let depth = self.config.cq_depth as i32;
        let (shared_qp, driver_handles, driver_futures) = if self.config.separate_cqs {
            let (send_cq, recv_cq) = match self.config.completion_mode {
                CompletionMode::Readiness => {
                    let sc = CqBuilder::new(&ctx, depth).with_channel().build()?;
                    let rc = CqBuilder::new(&ctx, depth).with_channel().build()?;
                    (sc, rc)
                }
                CompletionMode::Polling => {
                    let sc = CqBuilder::new(&ctx, depth).build()?;
                    let rc = CqBuilder::new(&ctx, depth).build()?;
                    (sc, rc)
                }
            };

            let cmqp = conn_id.create_qp_with_cq(
                pd.inner(),
                &self.qp_init_attr(),
                Some(send_cq.inner()),
                Some(recv_cq.inner()),
            )?;
            let qp = Qp::from_cm_qp(cmqp);

            let cap = self.config.inflight_capacity;
            let (send_handle, send_future) = self.create_driver(send_cq, cap);
            let (recv_handle, recv_future) = self.create_driver(recv_cq, cap);

            let sqp = SharedQp::new(
                qp,
                Arc::clone(&send_handle),
                Arc::clone(&recv_handle),
                pd.clone(),
            );
            (
                sqp,
                vec![send_handle, recv_handle],
                vec![send_future, recv_future],
            )
        } else {
            let cq = match self.config.completion_mode {
                CompletionMode::Readiness => CqBuilder::new(&ctx, depth).with_channel().build()?,
                CompletionMode::Polling => CqBuilder::new(&ctx, depth).build()?,
            };

            let cmqp = conn_id.create_qp_with_cq(
                pd.inner(),
                &self.qp_init_attr(),
                Some(cq.inner()),
                Some(cq.inner()),
            )?;
            let qp = Qp::from_cm_qp(cmqp);

            let cap = self.config.inflight_capacity;
            let (handle, future) = self.create_driver(cq, cap);

            let sqp = SharedQp::new(qp, Arc::clone(&handle), Arc::clone(&handle), pd.clone());
            (sqp, vec![handle], vec![future])
        };

        // Pre-establish hook
        pre_establish(&shared_qp, &pd)?;

        // Phased accept: reply (sync) → await ESTABLISHED → migrate
        conn_id.accept(&self.config.conn_param)?;
        listener.await_established().await?;

        let conn_ch = EventChannel::new()?;
        conn_ch.set_nonblocking()?;
        conn_id.migrate(&conn_ch)?;

        let conn_ch = Arc::new(conn_ch);
        let cm_async_fd = AsyncFd::new(conn_ch.fd()).map_err(Error::Verbs)?;

        Ok(ConnectionParts {
            shared_qp,
            driver_handles,
            driver_futures,
            resources: ConnectionResources {
                shared_qp: None,
                pd,
                cm_id: conn_id,
                event_channel: Arc::clone(&conn_ch),
            },
            cm_monitor_handle: Some(CmMonitorHandle {
                cm_async_fd,
                event_channel: conn_ch,
            }),
        })
    }

    /// Create a CQ driver future without spawning it.
    fn create_driver(
        &self,
        cq: super::cq::Cq,
        inflight_capacity: usize,
    ) -> (Arc<CqDriverHandle>, BoxedCqDriverFuture) {
        match self.config.completion_mode {
            CompletionMode::Readiness => {
                let (driver, handle) = FdCqDriver::new(cq, inflight_capacity);
                (handle, Box::pin(driver.run_tokio()))
            }
            CompletionMode::Polling => {
                let (driver, handle) = PollingCqDriver::new(cq, inflight_capacity);
                (handle, Box::pin(driver.run()))
            }
        }
    }

    fn qp_init_attr(&self) -> crate::qp::QpInitAttr {
        crate::qp::QpInitAttr {
            qp_type: crate::wr::QpType::Rc,
            max_send_wr: self.config.max_send_wr as u32,
            max_recv_wr: self.config.max_recv_wr as u32,
            max_send_sge: 1,
            max_recv_sge: 1,
            max_inline_data: 0,
            sq_sig_all: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation_zero_cq_depth() {
        let config = ConnectionConfig {
            completion_mode: CompletionMode::Readiness,
            cq_depth: 0,
            max_send_wr: 16,
            max_recv_wr: 16,
            inflight_capacity: 32,
            conn_param: ConnParam::default(),
            separate_cqs: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_zero_send_wr() {
        let config = ConnectionConfig {
            completion_mode: CompletionMode::Readiness,
            cq_depth: 64,
            max_send_wr: 0,
            max_recv_wr: 16,
            inflight_capacity: 32,
            conn_param: ConnParam::default(),
            separate_cqs: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_insufficient_inflight() {
        let config = ConnectionConfig {
            completion_mode: CompletionMode::Readiness,
            cq_depth: 64,
            max_send_wr: 16,
            max_recv_wr: 16,
            inflight_capacity: 16, // < 16 + 16
            conn_param: ConnParam::default(),
            separate_cqs: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_separate_cqs_no_inflight_check() {
        let config = ConnectionConfig {
            completion_mode: CompletionMode::Polling,
            cq_depth: 64,
            max_send_wr: 16,
            max_recv_wr: 16,
            inflight_capacity: 16, // < 32 but separate_cqs
            conn_param: ConnParam::default(),
            separate_cqs: true,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validation_valid() {
        let config = ConnectionConfig {
            completion_mode: CompletionMode::Readiness,
            cq_depth: 64,
            max_send_wr: 16,
            max_recv_wr: 16,
            inflight_capacity: 32,
            conn_param: ConnParam::default(),
            separate_cqs: false,
        };
        assert!(config.validate().is_ok());
    }
}
