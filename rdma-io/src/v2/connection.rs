//! Ergonomic v2 connection builder for RDMA transport setup.
//!
//! Provides [`Connection`] and [`CompletionMode`] for establishing RDMA
//! connections with automatic resource wiring.
//!
//! # Completion Modes
//!
//! - [`CompletionMode::Readiness`]: fd/channel-based CQ notification
//! - [`CompletionMode::Polling`]: direct CQ polling with cooperative yielding

use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::task::JoinHandle;

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
/// Does NOT own the `CmId` — that stays in [`Connection`] where
/// field-declaration-order drop guarantees QP is destroyed before CM ID.
pub(crate) struct CmMonitorHandle {
    pub(crate) cm_async_fd: AsyncFd<RawFd>,
    pub(crate) event_channel: Arc<EventChannel>,
}

/// An established RDMA connection with all resources wired.
///
/// # Drop Order
///
/// Fields drop in declaration order:
/// 1. `shared_qp` — QP destroyed (flushes outstanding WRs)
/// 2. `driver_handles` — `Arc<CqDriverHandle>` refs dropped
/// 3. `driver_tasks` — `JoinHandle`s aborted (synchronous, no await)
/// 4. `pd` — protection domain (ref-counted)
/// 5. `cm_id` — disconnect/destroy (AFTER QP)
/// 6. `event_channel` — closes fd (last, after CM ID)
pub struct Connection {
    shared_qp: SharedQp,
    driver_handles: Vec<Arc<CqDriverHandle>>,
    driver_tasks: Vec<JoinHandle<super::error::Result<()>>>,
    pd: Pd,
    #[expect(dead_code)] // held for drop ordering — must drop AFTER shared_qp
    cm_id: crate::cm::CmId,
    event_channel: Arc<EventChannel>,
    cm_async_fd: Option<AsyncFd<RawFd>>,
    shutdown_initiated: bool,
}

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
    ///
    /// The returned handle carries the `AsyncFd` and a shared ref to the
    /// `EventChannel`. The `CmId` stays in this `Connection` to ensure
    /// QP-before-CmId destruction order.
    pub(crate) fn take_cm_monitor_handle(&mut self) -> Option<CmMonitorHandle> {
        let async_fd = self.cm_async_fd.take()?;
        Some(CmMonitorHandle {
            cm_async_fd: async_fd,
            event_channel: Arc::clone(&self.event_channel),
        })
    }

    /// Synchronous, idempotent shutdown initiation. Safe for `Drop`.
    ///
    /// Transitions QP to error, flushes all inflight operations with
    /// synthetic errors, and aborts driver tasks. Does NOT await driver
    /// completion — use [`close()`](Self::close) for graceful shutdown.
    pub fn initiate_shutdown(&mut self) {
        if self.shutdown_initiated {
            return;
        }
        self.shutdown_initiated = true;
        let _ = self.shared_qp.shutdown();
        for handle in &self.driver_handles {
            handle.flush_and_shutdown();
        }
        // Abort only in Drop/sync context; close() awaits first
        for task in &self.driver_tasks {
            task.abort();
        }
    }

    /// Graceful async shutdown with bounded timeout.
    ///
    /// Transitions QP to error, flushes inflight operations, then awaits
    /// driver tasks (which perform final drain). Only aborts if a driver
    /// task exceeds the timeout.
    pub async fn close(mut self) {
        if !self.shutdown_initiated {
            self.shutdown_initiated = true;
            let _ = self.shared_qp.shutdown();
            for handle in &self.driver_handles {
                handle.flush_and_shutdown();
            }
            // Await driver tasks — let them drain, abort only on timeout
            for task in &mut self.driver_tasks {
                if tokio::time::timeout(std::time::Duration::from_secs(5), &mut *task)
                    .await
                    .is_err()
                {
                    task.abort();
                }
            }
        }
    }
}

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
    pub(crate) async fn connect<F>(self, addr: &SocketAddr, pre_establish: F) -> Result<Connection>
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

        // Build CQ(s)
        let depth = self.config.cq_depth as i32;
        let (shared_qp, driver_handles, driver_tasks) = if self.config.separate_cqs {
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

            // Build QP using CQ inner refs before drivers consume the CQs
            let cmqp = async_cm.create_qp_with_cq(
                pd.inner(),
                &self.qp_init_attr(),
                Some(send_cq.inner()),
                Some(recv_cq.inner()),
            )?;
            let qp = Qp::from_cm_qp(cmqp);

            // Create drivers (consumes CQs)
            let cap = self.config.inflight_capacity;
            let (send_handle, send_task) = self.spawn_driver(send_cq, cap)?;
            let (recv_handle, recv_task) = self.spawn_driver(recv_cq, cap)?;

            let sqp = SharedQp::new(
                qp,
                Arc::clone(&send_handle),
                Arc::clone(&recv_handle),
                pd.clone(),
            );
            (
                sqp,
                vec![send_handle, recv_handle],
                vec![send_task, recv_task],
            )
        } else {
            // Shared CQ mode: one CQ, one driver, same handle for both
            let cq = match self.config.completion_mode {
                CompletionMode::Readiness => CqBuilder::new(&ctx, depth).with_channel().build()?,
                CompletionMode::Polling => CqBuilder::new(&ctx, depth).build()?,
            };

            // Build QP: same CQ for both send and recv
            let cmqp = async_cm.create_qp_with_cq(
                pd.inner(),
                &self.qp_init_attr(),
                Some(cq.inner()),
                Some(cq.inner()),
            )?;
            let qp = Qp::from_cm_qp(cmqp);

            let cap = self.config.inflight_capacity;
            let (handle, task) = self.spawn_driver(cq, cap)?;

            let sqp = SharedQp::new(qp, Arc::clone(&handle), Arc::clone(&handle), pd.clone());
            (sqp, vec![handle], vec![task])
        };

        // Pre-establish hook
        pre_establish(&shared_qp, &pd)?;

        // Complete CM handshake
        async_cm.connect(&self.config.conn_param).await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(Error::Verbs)?;

        Ok(Connection {
            shared_qp,
            driver_handles,
            driver_tasks,
            pd,
            cm_id,
            event_channel: Arc::new(event_channel),
            cm_async_fd: Some(cm_async_fd),
            shutdown_initiated: false,
        })
    }

    /// Accept a connection (server side).
    pub(crate) async fn accept<F>(
        self,
        listener: &AsyncCmListener,
        pre_establish: F,
    ) -> Result<Connection>
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
        let (shared_qp, driver_handles, driver_tasks) = if self.config.separate_cqs {
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
            let (send_handle, send_task) = self.spawn_driver(send_cq, cap)?;
            let (recv_handle, recv_task) = self.spawn_driver(recv_cq, cap)?;

            let sqp = SharedQp::new(
                qp,
                Arc::clone(&send_handle),
                Arc::clone(&recv_handle),
                pd.clone(),
            );
            (
                sqp,
                vec![send_handle, recv_handle],
                vec![send_task, recv_task],
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
            let (handle, task) = self.spawn_driver(cq, cap)?;

            let sqp = SharedQp::new(qp, Arc::clone(&handle), Arc::clone(&handle), pd.clone());
            (sqp, vec![handle], vec![task])
        };

        // Pre-establish hook
        pre_establish(&shared_qp, &pd)?;

        // Phased accept: reply (sync) → await ESTABLISHED → migrate
        conn_id.accept(&self.config.conn_param)?;
        listener.await_established().await?;

        let conn_ch = EventChannel::new()?;
        conn_ch.set_nonblocking()?;
        conn_id.migrate(&conn_ch)?;

        let cm_async_fd = AsyncFd::new(conn_ch.fd()).map_err(Error::Verbs)?;

        Ok(Connection {
            shared_qp,
            driver_handles,
            driver_tasks,
            pd,
            cm_id: conn_id,
            event_channel: Arc::new(conn_ch),
            cm_async_fd: Some(cm_async_fd),
            shutdown_initiated: false,
        })
    }

    fn spawn_driver(
        &self,
        cq: super::cq::Cq,
        inflight_capacity: usize,
    ) -> Result<(Arc<CqDriverHandle>, JoinHandle<super::error::Result<()>>)> {
        match self.config.completion_mode {
            CompletionMode::Readiness => {
                let (driver, handle) = FdCqDriver::new(cq, inflight_capacity);
                let task = tokio::spawn(driver.run_tokio());
                Ok((handle, task))
            }
            CompletionMode::Polling => {
                let (driver, handle) = PollingCqDriver::new(cq, inflight_capacity);
                let task = tokio::spawn(driver.run());
                Ok((handle, task))
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
