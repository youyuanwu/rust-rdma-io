use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use tokio::sync::Mutex;

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
