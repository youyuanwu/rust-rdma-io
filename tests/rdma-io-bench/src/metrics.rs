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
