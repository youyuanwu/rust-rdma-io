use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

pub struct BenchMetrics {
    pub histograms: Vec<Mutex<Histogram<u64>>>,
    pub total_requests: AtomicU64,
    pub total_errors: AtomicU64,
    pub start_time: Instant,
}

impl BenchMetrics {
    pub fn new(num_connections: usize) -> Arc<Self> {
        let histograms = (0..num_connections)
            .map(|_| {
                Mutex::new(
                    Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("create histogram"),
                )
            })
            .collect();
        Arc::new(Self {
            histograms,
            total_requests: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            start_time: Instant::now(),
        })
    }

    pub async fn record(&self, conn_id: usize, latency: Duration) {
        let micros = latency.as_micros() as u64;
        let mut hist = self.histograms[conn_id].lock().await;
        let _ = hist.record(micros);
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_error(&self) {
        self.total_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn merged_histogram(&self) -> Histogram<u64> {
        let mut merged =
            Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("create histogram");
        for h in &self.histograms {
            merged.add(&*h.lock().await).expect("merge histograms");
        }
        merged
    }
}

/// Cumulative user+system CPU time of this process (summed over all threads),
/// in seconds, read from `/proc/self/stat`. Fields 14 (`utime`) and 15
/// (`stime`) are in clock ticks; Linux `USER_HZ` is 100 on these VMs.
pub fn process_cpu_seconds() -> f64 {
    const USER_HZ: f64 = 100.0;
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // The comm field (2nd) is wrapped in parens and may itself contain spaces
    // or parens, so split on the final ')': everything after it is space-
    // separated starting at field 3 (state).
    if let Some((_, rest)) = stat.rsplit_once(')') {
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // utime = field 14 -> index 11, stime = field 15 -> index 12.
        if fields.len() > 12 {
            let utime: u64 = fields[11].parse().unwrap_or(0);
            let stime: u64 = fields[12].parse().unwrap_or(0);
            return (utime + stime) as f64 / USER_HZ;
        }
    }
    0.0
}

/// Peak resident set size of this process in kilobytes (`VmHWM` high-water
/// mark), read from `/proc/self/status`.
pub fn peak_rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("VmHWM:") {
            return v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// Spawn a task that measures this process's CPU consumption strictly over the
/// `[warmup_deadline, bench_deadline]` window plus the peak RSS, so the reported
/// CPU-per-op excludes connection setup and warmup. Returns `(cpu_seconds,
/// peak_rss_kb)`. Shared by `--mode echo` and the tonic (`rh2`) / `tcp` paths.
pub fn spawn_resource_sampler(
    warmup_deadline: Instant,
    bench_deadline: Instant,
) -> tokio::task::JoinHandle<(f64, u64)> {
    tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(warmup_deadline)).await;
        let cpu_start = process_cpu_seconds();
        tokio::time::sleep_until(tokio::time::Instant::from_std(bench_deadline)).await;
        let cpu_end = process_cpu_seconds();
        ((cpu_end - cpu_start).max(0.0), peak_rss_kb())
    })
}

/// Per-connection task result routed to the readiness-barrier orchestrator:
/// `Ok(Ok((hist, errs)))` on success, `Ok(Err(_))` on a connect/loop error,
/// `Err(_)` on a panic.
type ConnJoinResult = Result<rdma_io::Result<(Histogram<u64>, u64)>, tokio::task::JoinError>;

/// Handed to each per-connection task: call [`report`](ConnReady::report) once
/// the transport + protocol handshake is complete, **before** awaiting the start
/// gate. Reporting is what lets the orchestrator start the shared clock only
/// after every connection is established.
pub struct ConnReady(mpsc::Sender<()>);

impl ConnReady {
    /// Signal that this connection has finished setup and is at the barrier.
    pub async fn report(self) {
        let _ = self.0.send(()).await;
    }
}

/// The shared start gate. Every per-connection task awaits [`wait`](StartGate::wait)
/// after reporting ready and only then begins its warmup/bench window, so the
/// measurement clock starts *after* all requested connections are established
/// (the readiness barrier, design §12 / §13 item 8).
pub struct StartGate(watch::Receiver<Option<(Instant, Instant)>>);

impl StartGate {
    /// Block until the orchestrator releases the barrier, returning the shared
    /// `(warmup_deadline, bench_deadline)`.
    pub async fn wait(mut self) -> (Instant, Instant) {
        loop {
            if let Some(v) = *self.0.borrow_and_update() {
                return v;
            }
            if self.0.changed().await.is_err() {
                // Orchestrator dropped the gate without releasing it (e.g. every
                // connection failed setup): return a zero-length window so the
                // task exits promptly instead of hanging.
                let now = Instant::now();
                return (now, now);
            }
        }
    }
}

/// Outcome of a readiness-barriered thread-per-core run: the merged latency
/// histogram, the total error count (failed / admission-refused connections
/// included), and the CPU/RSS sampled over the *post-barrier* window.
pub struct BarrierOutcome {
    pub merged: Histogram<u64>,
    pub errors: u64,
    pub cpu_seconds: f64,
    pub peak_rss_kb: u64,
}

fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("create histogram")
}

fn account_conn_result(merged: &mut Histogram<u64>, errors: &mut u64, res: ConnJoinResult) {
    match res {
        Ok(Ok((hist, errs))) => {
            *errors += errs;
            merged.add(&hist).expect("merge histograms");
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "thread-per-core connection failed");
            *errors += 1;
        }
        Err(e) => {
            tracing::warn!(error = %e, "thread-per-core connection task panicked");
            *errors += 1;
        }
    }
}

/// Orchestrate `connections` per-connection tasks behind a **readiness barrier**.
///
/// `spawn_one(ready, gate)` must spawn one connection's task on its owning core
/// and return its `JoinHandle` (or `None` if the pool refused admission). The
/// task must: finish transport + protocol setup, `ready.report().await`, then
/// `gate.wait().await` for the shared deadlines before running its loop.
///
/// The orchestrator waits until every requested connection is accounted for
/// (ready, admission-refused, or failed setup), and only **then** starts the
/// shared warmup/bench deadlines and the CPU/RSS sampler and releases the gate.
/// This removes the setup-time bias that skewed busy-vs-park comparisons (a
/// connection that set up late no longer runs a shorter, differently-timed
/// window, and setup CPU / CM retries stay out of the measured window). Failed
/// and refused connections are counted as errors; if fewer than `connections`
/// reach the barrier the run is flagged **RUN INVALID** on stderr.
pub async fn run_readiness_barrier<S>(
    connections: usize,
    warmup: u64,
    duration: u64,
    mut spawn_one: S,
) -> BarrierOutcome
where
    S: FnMut(ConnReady, StartGate) -> Option<JoinHandle<rdma_io::Result<(Histogram<u64>, u64)>>>,
{
    let cap = connections.max(1);
    let (ready_tx, mut ready_rx) = mpsc::channel::<()>(cap);
    let (res_tx, mut res_rx) = mpsc::channel::<ConnJoinResult>(cap);
    let (start_tx, start_rx) = watch::channel::<Option<(Instant, Instant)>>(None);

    let mut refused = 0usize;
    for _ in 0..connections {
        match spawn_one(ConnReady(ready_tx.clone()), StartGate(start_rx.clone())) {
            Some(handle) => {
                let res_tx = res_tx.clone();
                // Forward each task's outcome to the orchestrator whenever it
                // finishes: a healthy task blocks on the gate and so only
                // finishes after the barrier is released (phase 2), while a
                // failed connect finishes immediately (phase 1).
                tokio::spawn(async move {
                    let _ = res_tx.send(handle.await).await;
                });
            }
            None => refused += 1,
        }
    }
    // Drop our spare senders / receiver so the channels close once every task
    // (and its forwarder) has finished.
    drop(ready_tx);
    drop(res_tx);
    drop(start_rx);

    let mut merged = new_histogram();
    let mut errors = refused as u64;

    // Phase 1 — barrier: wait until every connection is accounted for. Any result
    // arriving here is a pre-start failure (a healthy task is parked on the gate).
    let mut ready = 0usize;
    let mut early = 0usize;
    while refused + ready + early < connections {
        tokio::select! {
            Some(()) = ready_rx.recv() => ready += 1,
            Some(res) = res_rx.recv() => {
                early += 1;
                account_conn_result(&mut merged, &mut errors, res);
            }
            else => break,
        }
    }

    // Release the barrier: start the shared clock + sampler now that setup is done.
    if ready < connections {
        eprintln!(
            "RUN INVALID: only {ready}/{connections} connection(s) reached the readiness \
             barrier; throughput is not comparable"
        );
    }
    let warmup_deadline = Instant::now() + Duration::from_secs(warmup);
    let bench_deadline = warmup_deadline + Duration::from_secs(duration);
    let sampler = spawn_resource_sampler(warmup_deadline, bench_deadline);
    let _ = start_tx.send(Some((warmup_deadline, bench_deadline)));
    eprintln!(
        "All {ready} connection(s) ready; warming up {warmup}s, then benchmarking {duration}s..."
    );

    // Phase 2 — collect the running connections' results.
    for _ in 0..ready {
        match res_rx.recv().await {
            Some(res) => account_conn_result(&mut merged, &mut errors, res),
            None => break,
        }
    }
    let (cpu_seconds, peak_rss_kb) = sampler.await.unwrap_or((0.0, 0));

    BarrierOutcome {
        merged,
        errors,
        cpu_seconds,
        peak_rss_kb,
    }
}
