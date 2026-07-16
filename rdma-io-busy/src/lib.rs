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
//!
//! # Two thread-per-core executors
//!
//! This crate ships two pinned per-core pools that differ only in **how
//! completions are delivered**:
//!
//! - [`BusyPool`] — **busy-poll**: one shared-CQ [`CoreDriver`] per core spins
//!   `ibv_poll_cq` at 100 %, the sole reaper for every connection on that core.
//!   Lowest latency, zero hot-path syscalls, but the core is pinned at 100 %
//!   even when idle. Connections use `connect_busy`/`accept_busy`.
//! - [`ArmParkPool`] — **thread-per-core arm-park**: the same pinned
//!   `current_thread` runtimes, but each connection keeps its own
//!   interrupt-armed CQ and the core **parks** in `epoll_wait` when idle
//!   (0 % idle CPU). Because each connection's completion-channel and CM fds
//!   bind to their owning core's reactor at construction time, the event-driven
//!   data path is serviced on that core with no cross-core work-stealing — the
//!   reactor-per-core model. Connections use the ordinary `connect`/`accept`.
//!   No shared CQ, so no `CoreDriver`, reclaim queue, or admission cap.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use rdma_io::async_cm::AsyncCmListener;
use rdma_io::core_driver::{CoreDriver, CoreDriverHandle};
use rdma_io::device::Context;
use rdma_io::read_ring_transport::{ReadRingConfig, ReadRingTransport};
use tokio::runtime::Handle as RtHandle;
use tokio::task::JoinHandle;

/// A pinned per-core `current_thread` runtime: pins a core, builds the runtime,
/// runs a caller-supplied `engine` future as the runtime's main task, and joins
/// the OS thread at shutdown. Both pools ([`BusyPool`], [`ArmParkPool`]) are
/// built on it; they differ only in the engine future (a [`CoreDriver`] run loop
/// vs a shutdown-signal await) and how they stop it.
struct PinnedRuntime {
    /// The core this runtime is pinned to (for observability).
    core_id: usize,
    /// Spawn point for on-core tasks (connections + their app loops).
    rt: RtHandle,
    /// The OS thread running the runtime; joined at shutdown.
    thread: Option<thread::JoinHandle<()>>,
}

impl PinnedRuntime {
    /// Join the worker thread if it hasn't been joined yet. Idempotent.
    ///
    /// The caller must first make the engine future return (stop the driver /
    /// send the stop signal) so the runtime's `block_on` completes.
    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One pinned per-core busy-poll worker: a [`PinnedRuntime`] plus the handle to
/// the [`CoreDriver`] running on it and its admission accounting.
struct CoreWorker {
    /// The pinned `current_thread` runtime + its OS thread (spawn point, join).
    inner: PinnedRuntime,
    /// The core's shared-CQ reaper handle (register/reclaim/shutdown).
    driver: CoreDriverHandle,
    /// Max live connections admitted to this core (`usize::MAX` = uncapped, for
    /// [`BusyPool::new`]; a finite cap for [`BusyPool::with_config`]).
    cap: usize,
    /// Current live-connection count (admission load). Incremented at placement,
    /// decremented by [`AdmissionGuard`] when the connection's task ends.
    admitted: Arc<AtomicUsize>,
}

/// Releases a core's admission slot when dropped. Held by the connection's task,
/// so the slot frees on the owning core when the app returns and the transport
/// is reclaimed.
struct AdmissionGuard {
    admitted: Arc<AtomicUsize>,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.admitted.fetch_sub(1, Ordering::AcqRel);
    }
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
        // Uncapped: placement is pure round-robin (admission always succeeds).
        Self::build(ctx, core_ids, send_depth, recv_depth, usize::MAX)
    }

    /// Build a pool that sizes each core's shared CQs for `conns_per_core`
    /// connections of `config` and **caps admission** at `conns_per_core` per
    /// core (Slice D3).
    ///
    /// The per-connection WR budget ([`ReadRingConfig::wr_budget`]) sets the
    /// shared-CQ depth: `(conns_per_core + 1) * per_conn_wrs` — the `+1` reserves
    /// **flush headroom** so a disconnecting connection's in-flight flush CQEs
    /// cannot overrun the CQ while a replacement is admitted (§7.2). Fails if
    /// that depth would exceed the device `max_cqe` (reduce `conns_per_core`,
    /// `ring_capacity`, or `max_in_flight`).
    pub fn with_config(
        ctx: Arc<Context>,
        core_ids: &[usize],
        config: &ReadRingConfig,
        conns_per_core: usize,
    ) -> rdma_io::Result<Self> {
        let budget = config.wr_budget(&ctx);
        // +1 connection's worth of headroom for flush CQEs during reclaim.
        let slots = (conns_per_core + 1) as i32;
        let send_depth = slots * budget.send_wrs;
        let recv_depth = slots * budget.recv_wrs;
        let max_cqe = ctx.query_device().map(|a| a.max_cqe).unwrap_or(i32::MAX);
        if send_depth > max_cqe || recv_depth > max_cqe {
            return Err(rdma_io::Error::InvalidArg(format!(
                "busy pool: conns_per_core={conns_per_core} needs shared CQ depth \
                 send={send_depth}/recv={recv_depth} > device max_cqe={max_cqe}; \
                 reduce conns_per_core / ring_capacity / max_in_flight"
            )));
        }
        Self::build(ctx, core_ids, send_depth, recv_depth, conns_per_core)
    }

    fn build(
        ctx: Arc<Context>,
        core_ids: &[usize],
        send_depth: i32,
        recv_depth: i32,
        cap: usize,
    ) -> rdma_io::Result<Self> {
        let mut workers: Vec<CoreWorker> = Vec::with_capacity(core_ids.len());
        for &core_id in core_ids {
            match spawn_worker(ctx.clone(), core_id, send_depth, recv_depth, cap) {
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
        self.workers.iter().map(|w| w.inner.core_id).collect()
    }

    /// Current live-connection count per core (in placement order). For tests /
    /// observability.
    pub fn admitted_counts(&self) -> Vec<usize> {
        self.workers
            .iter()
            .map(|w| w.admitted.load(Ordering::Acquire))
            .collect()
    }

    /// Reserve an admission slot on the next core with headroom (round-robin
    /// scan). Returns the worker index and a guard that releases the slot when
    /// dropped, or `None` if every core is at its cap (`with_config` pools only;
    /// an uncapped `new` pool always admits).
    fn try_admit(&self) -> Option<(usize, AdmissionGuard)> {
        let n = self.workers.len();
        for _ in 0..n {
            let i = self.next.fetch_add(1, Ordering::Relaxed) % n;
            let w = &self.workers[i];
            if w.admitted.fetch_add(1, Ordering::AcqRel) < w.cap {
                return Some((
                    i,
                    AdmissionGuard {
                        admitted: w.admitted.clone(),
                    },
                ));
            }
            // Over cap — undo and try the next core.
            w.admitted.fetch_sub(1, Ordering::AcqRel);
        }
        None
    }

    /// Open a busy-poll read-ring connection on the next core (round-robin) and
    /// run `app` with it **on that core**, returning a [`JoinHandle`] for the
    /// app's result.
    ///
    /// Both the `connect_busy` setup and `app` run on the owning core's runtime,
    /// so the transport never crosses a thread boundary (the owner-worker guard,
    /// §7.5). The transport is dropped when `app` returns, handing its resources
    /// to that core's driver reclaim queue (§6.2).
    ///
    /// Returns `None` if every core is at its admission cap (`with_config`
    /// pools); an uncapped [`new`](Self::new) pool always returns `Some`.
    pub fn spawn_connect<F, Fut, R>(
        &self,
        addr: SocketAddr,
        config: ReadRingConfig,
        app: F,
    ) -> Option<JoinHandle<rdma_io::Result<R>>>
    where
        F: FnOnce(ReadRingTransport) -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (i, guard) = self.try_admit()?;
        let worker = &self.workers[i];
        let driver = worker.driver.clone();
        Some(worker.inner.rt.spawn(async move {
            let mut transport = ReadRingTransport::connect_busy(&addr, config, &driver).await?;
            // Tie the admission lease to driver **retirement**, not app
            // completion: it travels into the reclaim bundle and frees the core's
            // admission slot only after the QP is drained + retired, so the shared
            // CQ can never be over-subscribed (review #2). A failed connect above
            // returns early and drops `guard`, releasing admission immediately.
            transport.set_reclaim_lease(Box::new(guard));
            Ok(app(transport).await)
        }))
    }

    /// Serve `count` busy-poll read-ring connections: accept each on the pool
    /// (round-robin across cores) and run `app` with it **on its owning core**.
    ///
    /// The accept *handshake* is **serialized** — the pool waits for each
    /// connection's setup to finish touching the listener before accepting the
    /// next — so the listener has a single consumer at a time and an
    /// `Established` can never be matched to the wrong connection (§6.1). This is
    /// the pragmatic realization of the "CM-ID-keyed control task": rather than a
    /// single task that routes concurrent handshakes' events by CM ID, one
    /// handshake is in flight at a time, so no routing is needed. The
    /// per-connection `app` loops run **concurrently** across cores once their
    /// own setup completes; only setup is serialized (it is not the hot path).
    ///
    /// Returns a [`JoinHandle`] per served connection; await them (each transport
    /// `Drop` then hands off to its core's reclaim queue) before
    /// [`shutdown`](Self::shutdown).
    pub async fn serve<F, Fut>(
        &self,
        listener: Arc<AsyncCmListener>,
        config: ReadRingConfig,
        count: usize,
        app: F,
    ) -> Vec<JoinHandle<()>>
    where
        F: Fn(ReadRingTransport) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut handles = Vec::with_capacity(count);
        for _ in 0..count {
            let Some((i, guard)) = self.try_admit() else {
                // Strict bound (review #2): never over-admit past a core's cap —
                // that would overrun the shared CQ sized for `cap + 1`. Reject the
                // pending accept instead of over-subscribing.
                tracing::error!(
                    "busy pool: no admission headroom for a pending accept; rejecting \
                     (server over-admission is bounded, review #2)"
                );
                break;
            };
            let worker = &self.workers[i];
            let driver = worker.driver.clone();
            let listener = listener.clone();
            let config = config.clone();
            let app = app.clone();
            // The worker signals when its handshake is done touching the
            // listener, so the pool can accept the next connection.
            let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<()>();
            let handle = worker.inner.rt.spawn(async move {
                let result = ReadRingTransport::accept_busy(&listener, config, &driver).await;
                let _ = setup_tx.send(());
                match result {
                    Ok(mut transport) => {
                        // Admission frees at retirement, not app end (review #2).
                        transport.set_reclaim_lease(Box::new(guard));
                        app(transport).await
                    }
                    // Accept failed: `guard` drops here, releasing admission.
                    Err(e) => tracing::warn!(error = %e, "busy pool accept failed"),
                }
            });
            handles.push(handle);
            // Serialize the handshake: a single listener consumer at a time.
            let _ = setup_rx.await;
        }
        handles
    }

    /// Stop every driver (draining its reclaim queue) and join the worker
    /// threads.
    ///
    /// **Close every connection first** (review #3): await each `JoinHandle`
    /// returned by [`spawn_connect`](Self::spawn_connect) / [`serve`](Self::serve)
    /// — aborting first if an app loop must be interrupted — so each transport
    /// `Drop` has already handed its resources to the owning driver's reclaim
    /// queue. Each driver then drains that queue before its thread joins. A
    /// `debug_assert` in the driver fires if a connection is still live at
    /// shutdown.
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
        w.inner.join();
    }
}

/// Spawn one pinned per-core `current_thread` runtime. Pins `core_id`, builds
/// the runtime, then runs `setup` **on-core** (so any CQs/MRs it allocates are
/// node-local, §4.1) to produce a ready payload `R` — sent back to the caller —
/// plus the `engine` future that keeps the runtime alive. The thread then
/// `block_on`s that engine; the runtime winds down when the engine returns.
fn spawn_pinned<S, R, Fut>(
    core_id: usize,
    thread_name: String,
    setup: S,
) -> rdma_io::Result<(PinnedRuntime, R)>
where
    S: FnOnce() -> Result<(R, Fut), String> + Send + 'static,
    R: Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    // Handshake: the worker sends back its runtime handle + ready payload (or
    // the error it hit) once `setup` has run on-core.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(RtHandle, R), String>>();

    let thread = thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            // Pin BEFORE building the runtime / allocating CQs/MRs (§4.1).
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

            rt.block_on(async move {
                let (payload, engine) = match setup() {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if ready_tx.send(Ok((rt_handle, payload))).is_err() {
                    // The pool gave up waiting; wind down cleanly (runtime drops
                    // on thread exit).
                    return;
                }
                engine.await;
            });
        })
        .map_err(|e| rdma_io::Error::InvalidArg(format!("spawn {thread_name}: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok((rt, payload))) => Ok((
            PinnedRuntime {
                core_id,
                rt,
                thread: Some(thread),
            },
            payload,
        )),
        Ok(Err(msg)) => {
            let _ = thread.join();
            Err(rdma_io::Error::InvalidArg(msg))
        }
        Err(_) => {
            let _ = thread.join();
            Err(rdma_io::Error::InvalidArg(format!(
                "{thread_name} thread exited before ready"
            )))
        }
    }
}

/// Spawn one pinned per-core busy-poll worker: a [`PinnedRuntime`] whose engine
/// future is the [`CoreDriver`] run loop, plus the driver handle + admission
/// accounting.
fn spawn_worker(
    ctx: Arc<Context>,
    core_id: usize,
    send_depth: i32,
    recv_depth: i32,
    cap: usize,
) -> rdma_io::Result<CoreWorker> {
    // The driver's run loop IS the runtime's main future: it services its own
    // task plus the app connections spawned onto the runtime, and returns only
    // after `shutdown()` + a fully drained reclaim queue (the shutdown join,
    // §10).
    let (inner, driver) = spawn_pinned(core_id, format!("busy-core-{core_id}"), move || {
        let (driver, handle) = CoreDriver::new(ctx, send_depth, recv_depth)
            .map_err(|e| format!("core driver: {e}"))?;
        Ok((handle, async move { driver.run().await }))
    })?;
    Ok(CoreWorker {
        inner,
        driver,
        cap,
        admitted: Arc::new(AtomicUsize::new(0)),
    })
}

// ---------------------------------------------------------------------------
// ArmParkPool — thread-per-core *arm-park* executor (no shared CoreDriver).
// ---------------------------------------------------------------------------

/// One pinned per-core worker running an *arm-park* `current_thread` runtime.
///
/// Unlike [`CoreWorker`] there is no `CoreDriver` — the runtime's engine future
/// is just a shutdown-signal await, and it drives the connection tasks spawned
/// onto its [`PinnedRuntime`] (each with its own interrupt-armed CQ) between
/// which the core parks in `epoll_wait`.
struct ParkWorker {
    /// The pinned `current_thread` runtime + its OS thread (spawn point, join).
    inner: PinnedRuntime,
    /// Ends the runtime's engine future at shutdown (drop or send). `Option` so
    /// [`shutdown_park_workers`] can take it before joining.
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

/// A thread-per-core **arm-park** pool: `N` pinned `current_thread` runtimes
/// (IO driver enabled) over which connections are sharded round-robin.
///
/// This is the interrupt-driven sibling of [`BusyPool`]. Instead of a shared
/// busy-polled CQ per core, each connection keeps its own arm-park CQ, and the
/// core **parks** in `epoll_wait` when idle (0 % idle CPU). The essential
/// mechanism is fd affinity: a connection built on core *c*'s runtime registers
/// its completion-channel and CM `AsyncFd`s with core *c*'s reactor, so all its
/// readiness events are serviced on core *c* — pinned locality without the 100 %
/// spin, and without tokio's cross-core work-stealing. There is no shared CQ, so
/// no `CoreDriver`, reclaim queue, or admission cap; a connection is a plain
/// `connect`/`accept` run on-core, dropped when its app returns.
pub struct ArmParkPool {
    workers: Vec<ParkWorker>,
    /// Round-robin placement cursor.
    next: AtomicUsize,
}

impl ArmParkPool {
    /// Build a pool with one pinned arm-park worker per entry in `core_ids`.
    pub fn new(core_ids: &[usize]) -> rdma_io::Result<Self> {
        let mut workers: Vec<ParkWorker> = Vec::with_capacity(core_ids.len());
        for &core_id in core_ids {
            match spawn_park_worker(core_id) {
                Ok(w) => workers.push(w),
                Err(e) => {
                    shutdown_park_workers(&mut workers);
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
        self.workers.iter().map(|w| w.inner.core_id).collect()
    }

    /// Pick the next core (round-robin).
    fn next_worker(&self) -> &ParkWorker {
        let n = self.workers.len();
        let i = self.next.fetch_add(1, Ordering::Relaxed) % n;
        &self.workers[i]
    }

    /// Open an arm-park read-ring connection on the next core (round-robin) and
    /// run `app` with it **on that core**, returning a [`JoinHandle`] for the
    /// app's result.
    ///
    /// The `connect` runs on the owning core's runtime, so the transport's
    /// completion-channel and CM fds bind to that core's reactor and its
    /// event-driven data path is serviced there. The transport is dropped when
    /// `app` returns (ordinary arm-park teardown drain, no reclaim queue).
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
        self.next_worker().inner.rt.spawn(async move {
            let transport = ReadRingTransport::connect(&addr, config).await?;
            Ok(app(transport).await)
        })
    }

    /// Serve `count` arm-park connections: accept each on the pool (round-robin
    /// across cores) and run `app` with it **on its owning core**.
    ///
    /// Like [`BusyPool::serve`], the accept *handshake* is **serialized** (the
    /// pool waits for each connection's setup to finish touching the listener
    /// before accepting the next), so the shared listener has a single consumer
    /// at a time and an `Established` cannot be matched to the wrong connection.
    /// The per-connection `app` loops run concurrently across cores once their
    /// own setup completes. The listener may live on a different (orchestration)
    /// runtime — its fd readiness is serviced there and wakes the on-core accept
    /// task via a cross-runtime waker; only the *accepted* transport's fds bind
    /// to the owning core.
    pub async fn serve<F, Fut>(
        &self,
        listener: Arc<AsyncCmListener>,
        config: ReadRingConfig,
        count: usize,
        app: F,
    ) -> Vec<JoinHandle<()>>
    where
        F: Fn(ReadRingTransport) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut handles = Vec::with_capacity(count);
        for _ in 0..count {
            let worker = self.next_worker();
            let rt = worker.inner.rt.clone();
            let listener = listener.clone();
            let config = config.clone();
            let app = app.clone();
            // The worker signals when its handshake is done touching the
            // listener, so the pool can accept the next connection.
            let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<()>();
            let handle = rt.spawn(async move {
                let result = ReadRingTransport::accept(&listener, config).await;
                let _ = setup_tx.send(());
                match result {
                    Ok(transport) => app(transport).await,
                    Err(e) => tracing::warn!(error = %e, "arm-park pool accept failed"),
                }
            });
            handles.push(handle);
            // Serialize the handshake: a single listener consumer at a time.
            let _ = setup_rx.await;
        }
        handles
    }

    /// Signal every worker's runtime to stop and join the threads. Call **after**
    /// every connection's `JoinHandle` has been awaited (a still-running task is
    /// cancelled when its runtime drops).
    pub fn shutdown(mut self) {
        shutdown_park_workers(&mut self.workers);
    }
}

impl Drop for ArmParkPool {
    fn drop(&mut self) {
        shutdown_park_workers(&mut self.workers);
    }
}

/// Signal each worker's engine future to end, then join its thread. Idempotent:
/// a `None` stop/thread (already taken) is skipped.
fn shutdown_park_workers(workers: &mut [ParkWorker]) {
    // Signal all first (drop/send the stop sender), then join — so the runtimes
    // wind down concurrently.
    for w in workers.iter_mut() {
        if let Some(tx) = w.stop.take() {
            let _ = tx.send(());
        }
    }
    for w in workers.iter_mut() {
        w.inner.join();
    }
}

/// Spawn one pinned per-core arm-park worker: a [`PinnedRuntime`] whose engine
/// future is a shutdown-signal await. The runtime drives the connection tasks
/// spawned onto it (each with its own interrupt-armed CQ); the core parks in
/// `epoll_wait` when idle (0 % CPU) between events.
fn spawn_park_worker(core_id: usize) -> rdma_io::Result<ParkWorker> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (inner, ()) = spawn_pinned(core_id, format!("park-core-{core_id}"), move || {
        Ok(((), async move {
            let _ = stop_rx.await;
        }))
    })?;
    Ok(ParkWorker {
        inner,
        stop: Some(stop_tx),
    })
}
