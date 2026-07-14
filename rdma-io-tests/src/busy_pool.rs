//! `BusyPool` — a harness-layer per-core worker pool for busy-poll (Slice D1).
//!
//! This is the **thread-per-core** executor the busy-poll design (§4.1, §10
//! "Slice D design") calls for, kept *out* of the runtime-agnostic `rdma-io`
//! crate on purpose: runtime topology (which cores to pin, thread lifecycle,
//! shutdown) is deployment policy, so the pool lives in the harness layer and
//! wraps the primitives `rdma-io` already ships (`CoreDriver` +
//! `connect_busy`/`accept_busy`).
//!
//! Each core gets its own OS thread running a pinned `current_thread` tokio
//! runtime with exactly one [`CoreDriver`] task. A connection is placed on a
//! core (round-robin) and is **core-affine for life** (§7.5): both its
//! `connect_busy` setup *and* its application loop run on that core's runtime,
//! next to the driver that reaps its shared CQ. That co-location is mandatory —
//! the `ConnSlot` inbox is same-core SPSC and the transport carries an
//! owner-worker guard — so the pool never hands a transport back to the caller;
//! [`BusyPool::spawn_connect`] runs the application closure **on-core** and
//! returns only its result via a [`JoinHandle`].
//!
//! Teardown follows the reclaim protocol: await every connection's
//! `JoinHandle` (so each transport `Drop` hands its resources to the owning
//! driver's reclaim queue), then [`BusyPool::shutdown`] stops each driver, which
//! drains its reclaim queue before its thread joins.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use rdma_io::core_driver::{CoreDriver, CoreDriverHandle};
use rdma_io::device::Context;
use rdma_io::read_ring_transport::{ReadRingConfig, ReadRingTransport};
use tokio::runtime::Handle as RtHandle;
use tokio::task::JoinHandle;

/// One pinned per-core worker: a `current_thread` runtime handle plus the
/// handle to the [`CoreDriver`] running on it.
struct CoreWorker {
    /// The core this worker is pinned to (for observability).
    core_id: usize,
    /// Spawn point for on-core tasks (connections + their app loops).
    rt: RtHandle,
    /// The core's shared-CQ reaper handle (register/reclaim/shutdown).
    driver: CoreDriverHandle,
    /// The OS thread running the runtime; joined at shutdown.
    thread: Option<thread::JoinHandle<()>>,
}

/// A pool of pinned per-core busy-poll workers, over which connections are
/// sharded round-robin. See the module docs.
pub struct BusyPool {
    workers: Vec<CoreWorker>,
    /// Round-robin placement cursor.
    next: AtomicUsize,
}

impl BusyPool {
    /// Build a pool with one pinned worker per entry in `core_ids`, each owning
    /// a [`CoreDriver`] with `send_depth`/`recv_depth`-entry shared CQs on the
    /// shared device `ctx`.
    ///
    /// `ctx` is the per-device verbs context (librdmacm shares one per device,
    /// so all cores' CQs live on it). Obtain it once from a probe `cm_id` that
    /// resolved the target address, and keep that `cm_id` alive for the pool's
    /// lifetime.
    ///
    /// The core is pinned **before** the driver allocates its CQs (§4.1), so the
    /// poll loop, CQ ring, and DMA buffers are node-local.
    pub fn new(
        ctx: Arc<Context>,
        core_ids: &[usize],
        send_depth: i32,
        recv_depth: i32,
    ) -> rdma_io::Result<Self> {
        let mut workers: Vec<CoreWorker> = Vec::with_capacity(core_ids.len());
        for &core_id in core_ids {
            match spawn_worker(ctx.clone(), core_id, send_depth, recv_depth) {
                Ok(w) => workers.push(w),
                Err(e) => {
                    // Clean up the workers already started before failing.
                    shutdown_workers(&mut workers);
                    return Err(e);
                }
            }
        }
        Ok(Self {
            workers,
            next: AtomicUsize::new(0),
        })
    }

    /// Number of pinned cores in the pool.
    pub fn core_count(&self) -> usize {
        self.workers.len()
    }

    /// The core ids the pool is pinned to, in placement order.
    pub fn core_ids(&self) -> Vec<usize> {
        self.workers.iter().map(|w| w.core_id).collect()
    }

    /// Round-robin the next worker.
    fn next_worker(&self) -> &CoreWorker {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        &self.workers[i]
    }

    /// Open a busy-poll read-ring connection on the next core (round-robin) and
    /// run `app` with it **on that core**, returning a [`JoinHandle`] for the
    /// app's result.
    ///
    /// Both the `connect_busy` setup and `app` run on the owning core's runtime,
    /// so the transport never crosses a thread boundary (the owner-worker guard,
    /// §7.5). The transport is dropped when `app` returns, handing its resources
    /// to that core's driver reclaim queue (§6.2).
    pub fn spawn_connect<F, Fut, R>(
        &self,
        addr: SocketAddr,
        config: ReadRingConfig,
        app: F,
    ) -> JoinHandle<rdma_io::Result<R>>
    where
        F: FnOnce(ReadRingTransport) -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let worker = self.next_worker();
        let driver = worker.driver.clone();
        worker.rt.spawn(async move {
            let transport = ReadRingTransport::connect_busy(&addr, config, &driver).await?;
            Ok(app(transport).await)
        })
    }

    /// Stop every driver (draining its reclaim queue) and join the worker
    /// threads. Call **after** every connection's `JoinHandle` has been awaited,
    /// so each transport `Drop` has already enqueued its reclaim (§6.2).
    pub fn shutdown(mut self) {
        shutdown_workers(&mut self.workers);
    }
}

impl Drop for BusyPool {
    fn drop(&mut self) {
        shutdown_workers(&mut self.workers);
    }
}

/// Signal each driver to stop and join its thread. Idempotent: a `None` thread
/// handle (already joined) is skipped.
fn shutdown_workers(workers: &mut [CoreWorker]) {
    // Signal all first, then join — so the drivers wind down concurrently.
    for w in workers.iter() {
        w.driver.shutdown();
    }
    for w in workers.iter_mut() {
        if let Some(t) = w.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawn one pinned worker thread: pin the core, build a `current_thread`
/// runtime, create + run the [`CoreDriver`] on it, and hand back the runtime
/// handle + driver handle once the driver is up.
fn spawn_worker(
    ctx: Arc<Context>,
    core_id: usize,
    send_depth: i32,
    recv_depth: i32,
) -> rdma_io::Result<CoreWorker> {
    // Handshake: the worker sends back its runtime + driver handle (or the error
    // it hit) once the driver task is running.
    let (ready_tx, ready_rx) =
        std::sync::mpsc::channel::<Result<(RtHandle, CoreDriverHandle), String>>();

    let thread = thread::Builder::new()
        .name(format!("busy-core-{core_id}"))
        .spawn(move || {
            // Pin BEFORE allocating CQs/MRs so they are node-local (§4.1).
            let _ = core_affinity::set_for_current(core_affinity::CoreId { id: core_id });

            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("build runtime: {e}")));
                    return;
                }
            };
            let rt_handle = rt.handle().clone();

            // The driver's run loop IS this runtime's main future: it services
            // its own task plus the app connections spawned onto `rt_handle`,
            // and returns only after `shutdown()` + a fully drained reclaim
            // queue (the shutdown join, §10).
            rt.block_on(async move {
                let (driver, handle) = match CoreDriver::new(ctx, send_depth, recv_depth) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("core driver: {e}")));
                        return;
                    }
                };
                if ready_tx.send(Ok((rt_handle, handle))).is_err() {
                    // The pool gave up waiting; still wind down cleanly by
                    // returning (the runtime drops on thread exit).
                    return;
                }
                driver.run().await;
            });
        })
        .map_err(|e| rdma_io::Error::InvalidArg(format!("spawn busy-core-{core_id}: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok((rt, driver))) => Ok(CoreWorker {
            core_id,
            rt,
            driver,
            thread: Some(thread),
        }),
        Ok(Err(msg)) => {
            let _ = thread.join();
            Err(rdma_io::Error::InvalidArg(msg))
        }
        Err(_) => {
            let _ = thread.join();
            Err(rdma_io::Error::InvalidArg(format!(
                "busy-core-{core_id} thread exited before ready"
            )))
        }
    }
}
