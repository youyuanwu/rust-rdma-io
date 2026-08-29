//! CQ completion drivers for per-operation future routing.
//!
//! Provides async tasks that drain an RDMA completion queue and dispatch
//! each CQE to the corresponding per-operation future via [`InflightMap`].
//!
//! Two driver modes match the v2 dual-CQ integration model:
//! - [`FdCqDriver`]: fd/readiness-based, using a `CqNotifier` for async runtime
//!   reactor integration (arm-drain pattern)
//! - [`PollingCqDriver`]: direct CQ polling with bounded poll budget and
//!   cooperative yielding

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::wc::WorkCompletion;

use super::cq::Cq;
use super::inflight::InflightMap;

/// Shared state between operation futures and the completion driver.
///
/// Created by [`CqDriver::new()`] and shared via `Arc`.
pub struct CqDriverHandle {
    /// The inflight operation registry.
    pub(crate) map: InflightMap,
    /// Signal to stop the driver loop.
    shutdown: AtomicBool,
}

impl CqDriverHandle {
    /// Signal the driver to shut down.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Check if shutdown was requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Access the inflight map for operation registration.
    pub fn map(&self) -> &InflightMap {
        &self.map
    }
}

/// Fd/readiness-based completion driver.
///
/// Uses a `CqNotifier` to await CQ readiness via the completion channel fd,
/// then drains the CQ and routes completions to registered futures.
///
/// Spawn this as a task on your async runtime:
/// ```no_run
/// # use rdma_io::v2::*;
/// # async fn example(ctx: &Context) -> Result<()> {
/// let cq = CqBuilder::new(ctx, 64).with_channel().build()?;
/// let (driver, handle) = FdCqDriver::new(cq, 64);
/// let driver_task = tokio::spawn(driver.run_tokio());
/// // ... use handle to create SharedQp and submit operations ...
/// handle.shutdown();
/// driver_task.await.ok();
/// # Ok(())
/// # }
/// ```
pub struct FdCqDriver {
    cq: Cq,
    handle: Arc<CqDriverHandle>,
}

impl FdCqDriver {
    /// Create a new fd-based driver and its shared handle.
    ///
    /// `inflight_capacity` is the max number of concurrent in-flight
    /// operations this driver can track.
    pub fn new(cq: Cq, inflight_capacity: usize) -> (Self, Arc<CqDriverHandle>) {
        let handle = Arc::new(CqDriverHandle {
            map: InflightMap::new(inflight_capacity),
            shutdown: AtomicBool::new(false),
        });
        (
            Self {
                cq,
                handle: Arc::clone(&handle),
            },
            handle,
        )
    }

    /// Run the driver loop with Tokio's CQ notifier.
    ///
    /// This is the primary entry point — spawn this on a Tokio runtime.
    /// The loop exits when `handle.shutdown()` is called.
    #[cfg(feature = "tokio")]
    pub async fn run_tokio(self) -> super::error::Result<()> {
        let fd = self.cq.fd().ok_or_else(|| {
            super::error::Error::InvalidConfig("FdCqDriver requires a channel-backed CQ".into())
        })?;
        let notifier =
            crate::tokio_notifier::TokioCqNotifier::new(fd).map_err(super::error::Error::Verbs)?;
        self.run(notifier).await
    }

    /// Run the driver loop with a custom `CqNotifier`.
    pub async fn run<N: crate::async_cq::CqNotifier>(
        self,
        notifier: N,
    ) -> super::error::Result<()> {
        let mut wc_buf = [WorkCompletion::default(); 32];

        while !self.handle.is_shutdown() {
            // Arm CQ notification
            self.cq.inner().req_notify(false)?;

            // Drain any pending completions
            let n = self.cq.poll(&mut wc_buf)?;
            if n > 0 {
                self.dispatch(&wc_buf[..n]);
                continue;
            }

            // Wait for fd readiness
            if let Err(e) = notifier.readable().await {
                if self.handle.is_shutdown() {
                    break;
                }
                return Err(super::error::Error::Verbs(e));
            }

            // Drain completion channel events and ack
            if let Some(ch) = self.cq.channel() {
                drain_channel(ch, self.cq.inner().as_raw());
            }

            // Drain CQ after wakeup
            loop {
                let n = self.cq.poll(&mut wc_buf)?;
                if n == 0 {
                    break;
                }
                self.dispatch(&wc_buf[..n]);
            }
        }

        Ok(())
    }

    fn dispatch(&self, wcs: &[WorkCompletion]) {
        for wc in wcs {
            let token = wc.wr_id();
            if !self.handle.map.complete(token, *wc) {
                tracing::debug!(
                    token,
                    "driver: unroutable completion (stale or unknown token)"
                );
            }
        }
    }
}

/// Polling-based completion driver.
///
/// Directly polls the RDMA CQ with a bounded poll budget per iteration,
/// then yields to the async runtime. Similar to the v1 `CoreDriver` sweep
/// pattern but operates as a cooperative async task, not a busy thread.
///
/// ```no_run
/// # use rdma_io::v2::*;
/// # async fn example(ctx: &Context) -> Result<()> {
/// let cq = CqBuilder::new(ctx, 64).build()?; // poll-only CQ
/// let (driver, handle) = PollingCqDriver::new(cq, 64);
/// let driver_task = tokio::spawn(driver.run());
/// // ... use handle to create SharedQp and submit operations ...
/// handle.shutdown();
/// driver_task.await.ok();
/// # Ok(())
/// # }
/// ```
pub struct PollingCqDriver {
    cq: Cq,
    handle: Arc<CqDriverHandle>,
    /// Max CQEs to drain per poll iteration before yielding.
    poll_budget: usize,
}

impl PollingCqDriver {
    /// Create a new polling driver and its shared handle.
    pub fn new(cq: Cq, inflight_capacity: usize) -> (Self, Arc<CqDriverHandle>) {
        let handle = Arc::new(CqDriverHandle {
            map: InflightMap::new(inflight_capacity),
            shutdown: AtomicBool::new(false),
        });
        (
            Self {
                cq,
                handle: Arc::clone(&handle),
                poll_budget: 32,
            },
            handle,
        )
    }

    /// Set the maximum number of CQEs to drain per poll iteration.
    ///
    /// After draining this many CQEs (or the CQ is empty), the driver
    /// yields to the async runtime to allow other tasks to run.
    /// Default: 32.
    pub fn poll_budget(mut self, budget: usize) -> Self {
        self.poll_budget = budget.max(1);
        self
    }

    /// Run the polling driver loop.
    ///
    /// Continuously polls the CQ with bounded budget, dispatches completions,
    /// and yields cooperatively. Exits when `handle.shutdown()` is called.
    pub async fn run(self) -> super::error::Result<()> {
        let mut wc_buf = vec![WorkCompletion::default(); self.poll_budget];

        while !self.handle.is_shutdown() {
            let n = self.cq.poll(&mut wc_buf)?;
            if n > 0 {
                self.dispatch(&wc_buf[..n]);
            } else {
                // No completions — yield to runtime
                tokio::task::yield_now().await;
            }
        }

        // Final drain on shutdown
        loop {
            let n = self.cq.poll(&mut wc_buf)?;
            if n == 0 {
                break;
            }
            self.dispatch(&wc_buf[..n]);
        }

        Ok(())
    }

    fn dispatch(&self, wcs: &[WorkCompletion]) {
        for wc in wcs {
            let token = wc.wr_id();
            if !self.handle.map.complete(token, *wc) {
                tracing::debug!(token, "polling driver: unroutable completion");
            }
        }
    }
}

/// Drain all pending events from a completion channel and ack them.
fn drain_channel(
    ch: &crate::comp_channel::CompletionChannel,
    cq_raw: *mut rdma_io_sys::ibverbs::ibv_cq,
) {
    let mut count = 0u32;
    loop {
        match ch.get_cq_event() {
            Ok(_) => {
                count += 1;
            }
            Err(crate::Error::WouldBlock) => break,
            Err(crate::Error::Verbs(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                break;
            }
            Err(e) => {
                tracing::warn!("drain_channel error: {e}");
                break;
            }
        }
    }
    if count > 0 {
        unsafe {
            rdma_io_sys::ibverbs::ibv_ack_cq_events(cq_raw, count);
        }
    }
}
