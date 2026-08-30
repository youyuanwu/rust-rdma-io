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
//!
//! # Centralized Cancellation Reclamation
//!
//! When an [`OpFuture`](super::shared_qp::OpFuture) is dropped while in-flight,
//! the owned MR and registry slot are pushed to the driver's reclaim queue
//! instead of spawning a detached task. The driver drains the reclaim queue
//! each loop turn, releasing resources when their CQE arrives.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::wc::WorkCompletion;

use super::cq::Cq;
use super::inflight::InflightMap;
use super::mr::Mr;

/// Maximum time a detached operation may wait for its CQE before
/// being quarantined. Uses wall-clock time instead of loop iterations
/// to avoid false quarantines under normal scheduling latency.
const RECLAIM_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Recovery interval while a readiness driver has operations in flight.
///
/// Completion-channel notification is still the primary wake source. The
/// bounded poll prevents a missed provider notification from parking an
/// operation forever.
const READINESS_POLL_FALLBACK: std::time::Duration = std::time::Duration::from_millis(100);

/// A detached in-flight operation awaiting reclamation.
pub(crate) struct DetachedOp {
    /// The registry token for this operation.
    pub(crate) token: u64,
    /// The owned MR kept alive while the HCA may still reference it.
    pub(crate) mr: Option<Mr>,
    /// Optional callback invoked with the reclaimed MR.
    pub(crate) on_reclaim: Option<Box<dyn FnOnce(Mr) + Send>>,
    /// Number of drain passes this entry has survived.
    pub(crate) turns: usize,
    /// Timestamp when this operation entered the reclaim queue.
    pub(crate) created_at: tokio::time::Instant,
    /// When true, the MR is quarantined — kept alive until CqDriverHandle
    /// drops (which structurally follows QP destruction). The registry slot
    /// is released but the MR and entry remain in the queue.
    pub(crate) quarantined: bool,
}

/// Shared state between operation futures and the completion driver.
///
/// Created by driver constructors and shared via `Arc`. Owns the in-flight
/// operation registry and a reclaim queue for cancelled operations.
pub struct CqDriverHandle {
    /// The inflight operation registry.
    pub(crate) map: InflightMap,
    /// Signal to stop the driver loop.
    shutdown: AtomicBool,
    /// Queue of detached operations awaiting reclamation.
    reclaim_queue: Mutex<Vec<DetachedOp>>,
    /// Notify to wake an idle FdCqDriver when entries are pushed
    /// to the reclaim queue or when shutdown is requested.
    #[cfg(feature = "tokio")]
    pub(crate) shutdown_notify: tokio::sync::Notify,
    #[cfg(feature = "tokio")]
    pub(crate) reclaim_notify: tokio::sync::Notify,
    /// Coalesced notification that a new WR was posted.
    #[cfg(feature = "tokio")]
    work_notify: tokio::sync::Notify,
}

impl CqDriverHandle {
    /// Create a new driver handle with the given inflight capacity.
    pub(crate) fn new(inflight_capacity: usize) -> Self {
        Self {
            map: InflightMap::new(inflight_capacity),
            shutdown: AtomicBool::new(false),
            reclaim_queue: Mutex::new(Vec::new()),
            #[cfg(feature = "tokio")]
            shutdown_notify: tokio::sync::Notify::new(),
            #[cfg(feature = "tokio")]
            reclaim_notify: tokio::sync::Notify::new(),
            #[cfg(feature = "tokio")]
            work_notify: tokio::sync::Notify::new(),
        }
    }

    /// Wake the readiness driver after posting a work request.
    pub(crate) fn notify_work(&self) {
        #[cfg(feature = "tokio")]
        self.work_notify.notify_one();
    }

    /// Signal the driver to shut down and wake any blocked driver loop.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        #[cfg(feature = "tokio")]
        self.shutdown_notify.notify_waiters();
    }

    /// Check if shutdown was requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Access the inflight map for operation registration.
    pub fn map(&self) -> &InflightMap {
        &self.map
    }

    /// Push a detached operation for background reclamation.
    ///
    /// Fast path: if the completion has already arrived, the slot is
    /// released immediately and the `on_reclaim` callback is invoked
    /// (or the MR is dropped). No enqueue occurs.
    ///
    /// Slow path: the entry is added to the reclaim queue and the
    /// driver is notified to drain it.
    pub(crate) fn push_detached(
        &self,
        token: u64,
        mr: Mr,
        on_reclaim: Option<Box<dyn FnOnce(Mr) + Send>>,
    ) {
        // Fast path: completion already arrived
        if self.map.take_completion(token).is_some() {
            self.map.release(token);
            if let Some(cb) = on_reclaim {
                cb(mr);
            }
            return;
        }

        // Slow path: enqueue for driver to drain
        {
            let mut queue = self.reclaim_queue.lock().unwrap();
            queue.push(DetachedOp {
                token,
                mr: Some(mr),
                on_reclaim,
                turns: 0,
                created_at: tokio::time::Instant::now(),
                quarantined: false,
            });
        }
        #[cfg(feature = "tokio")]
        self.reclaim_notify.notify_one();
    }

    /// Drain the reclaim queue, releasing entries whose real CQE has arrived.
    ///
    /// Unlike previous versions, this method does NOT force-release entries
    /// on shutdown. Entries without a real CQE stay in the queue — their MRs
    /// are freed only when `CqDriverHandle` drops (which structurally occurs
    /// after QP destruction via `ConnectionLifetime` field ordering).
    ///
    /// On `RECLAIM_DEADLINE` exceeded (wedged provider), the registry slot
    /// is released but the MR is quarantined in the queue for safe
    /// destruction when the handle drops.
    ///
    /// Returns the number of entries still pending.
    pub(crate) fn drain_reclaimed(&self) -> usize {
        let mut queue = self.reclaim_queue.lock().unwrap();

        queue.retain_mut(|entry| {
            // Quarantined entries stay until CqDriverHandle drops
            if entry.quarantined {
                return true;
            }

            entry.turns += 1;

            // Check if real completion arrived
            if self.map.take_completion(entry.token).is_some() {
                self.map.release(entry.token);
                if let Some(cb) = entry.on_reclaim.take()
                    && let Some(mr) = entry.mr.take()
                {
                    cb(mr);
                }
                return false; // remove from queue
            }

            // Wedge escape hatch: release registry slot but QUARANTINE MR.
            // The MR stays alive in this queue entry and is only freed
            // when CqDriverHandle drops — which structurally follows QP
            // destruction per ConnectionLifetime field ordering.
            if entry.created_at.elapsed() >= RECLAIM_DEADLINE {
                tracing::error!(
                    token = entry.token,
                    turns = entry.turns,
                    elapsed_ms = entry.created_at.elapsed().as_millis(),
                    "reclaim entry exceeded deadline — quarantining MR (buffer pool permanently shrunk)"
                );
                self.map.release(entry.token);
                entry.on_reclaim.take();
                entry.quarantined = true;
                return true;
            }

            true // keep in queue
        });

        queue.len()
    }

    /// Close the inflight map and signal shutdown.
    ///
    /// Wakes all registered waiters so they can quarantine their MRs
    /// (push to reclaim queue) rather than returning them to callers.
    /// Does NOT write synthetic completions — MRs are released only
    /// when real CQEs arrive or when the QP is destroyed (via
    /// `ConnectionLifetime` drop ordering).
    ///
    /// # Safety Invariant
    ///
    /// An MR posted to hardware may be returned/reused/dropped only after
    /// its actual CQE is reaped OR the owning QP has been synchronously
    /// destroyed. This method enforces the invariant by closing the map
    /// (preventing MR return via OpFuture) without releasing MRs.
    pub fn close_and_shutdown(&self) {
        self.map.close();
        self.shutdown();
    }

    /// Number of entries currently in the reclaim queue.
    #[cfg(test)]
    #[expect(dead_code)]
    pub(crate) fn reclaim_len(&self) -> usize {
        self.reclaim_queue.lock().unwrap().len()
    }

    /// Number of non-quarantined entries in the reclaim queue.
    ///
    /// Quarantined entries are kept alive for safe destruction and should
    /// not block drain barrier exit.
    pub(crate) fn active_reclaim_count(&self) -> usize {
        self.reclaim_queue
            .lock()
            .unwrap()
            .iter()
            .filter(|e| !e.quarantined)
            .count()
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
        let handle = Arc::new(CqDriverHandle::new(inflight_capacity));
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
                self.handle.drain_reclaimed();
                if let Some(ch) = self.cq.channel() {
                    drain_channel(ch, self.cq.inner().as_raw());
                }
                continue;
            }

            // Drain reclaim queue before parking
            self.handle.drain_reclaimed();

            // Re-check shutdown after work phase to close the window between
            // the loop-head check and the select!'s Notified snapshot (same
            // register-check-recheck pattern used in message_transport send()).
            if self.handle.is_shutdown() {
                break;
            }

            // Wait for fd readiness, shutdown, or reclaim notification
            tokio::select! {
                biased;
                _ = self.handle.shutdown_notify.notified() => {
                    break;
                }
                _ = self.handle.reclaim_notify.notified() => {
                    // Reclaim entries pushed — drain and re-loop
                    self.handle.drain_reclaimed();
                    continue;
                }
                _ = self.handle.work_notify.notified() => {
                    // A WR was posted while the driver was parked. Re-poll now;
                    // if it is not complete yet, the in-flight fallback below
                    // prevents a missed provider event from parking forever.
                    continue;
                }
                result = notifier.readable() => {
                    if let Err(e) = result {
                        if self.handle.is_shutdown() {
                            break;
                        }
                        return Err(super::error::Error::Verbs(e));
                    }
                }
                _ = tokio::time::sleep(READINESS_POLL_FALLBACK),
                    if self.handle.map.inflight_count() > 0 =>
                {
                    continue;
                }
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
            self.handle.drain_reclaimed();
        }

        // Final drain barrier: cooperatively poll CQ for real flush CQEs
        // until quiescent or deadline. Yields every 32 iterations to remain
        // cooperative and allow DRAIN_TIMEOUT in Phase C to fire.
        let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut drain_iter = 0usize;
        loop {
            let n = self.cq.poll(&mut wc_buf)?;
            if n > 0 {
                self.dispatch(&wc_buf[..n]);
            }
            self.handle.drain_reclaimed();
            let active_reclaim = self.handle.active_reclaim_count();
            let inflight = self.handle.map.inflight_count();
            if n == 0 && active_reclaim == 0 && inflight == 0 {
                break;
            }
            drain_iter += 1;
            if tokio::time::Instant::now() >= drain_deadline {
                tracing::warn!(
                    inflight,
                    active_reclaim,
                    "FdCqDriver: drain deadline reached"
                );
                break;
            }
            if drain_iter.is_multiple_of(32) {
                tokio::task::yield_now().await;
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
        let handle = Arc::new(CqDriverHandle::new(inflight_capacity));
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
        let mut drained_this_round = 0usize;

        while !self.handle.is_shutdown() {
            let n = self.cq.poll(&mut wc_buf)?;
            if n > 0 {
                self.dispatch(&wc_buf[..n]);
                drained_this_round += n;
                // Yield after draining budget to stay cooperative
                if drained_this_round >= self.poll_budget {
                    drained_this_round = 0;
                    self.handle.drain_reclaimed();
                    tokio::task::yield_now().await;
                }
            } else {
                // No completions — yield to runtime
                drained_this_round = 0;
                self.handle.drain_reclaimed();
                tokio::task::yield_now().await;
            }
        }

        let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut drain_iter = 0usize;
        loop {
            let n = self.cq.poll(&mut wc_buf)?;
            if n > 0 {
                self.dispatch(&wc_buf[..n]);
            }
            self.handle.drain_reclaimed();
            let active_reclaim = self.handle.active_reclaim_count();
            let inflight = self.handle.map.inflight_count();
            if n == 0 && active_reclaim == 0 && inflight == 0 {
                break;
            }
            drain_iter += 1;
            if tokio::time::Instant::now() >= drain_deadline {
                tracing::warn!(
                    inflight,
                    active_reclaim,
                    "PollingCqDriver: drain deadline reached"
                );
                break;
            }
            if drain_iter.is_multiple_of(32) {
                tokio::task::yield_now().await;
            }
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
