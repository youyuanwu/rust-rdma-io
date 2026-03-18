use hdrhistogram::Histogram;
use serde::Serialize;

#[derive(Serialize)]
pub struct BenchResult {
    pub mode: String,
    pub transport: String,
    pub connections: usize,
    pub threads: usize,
    pub duration_secs: u64,
    pub payload_bytes: usize,
    pub total_requests: u64,
    pub throughput_rps: f64,
    pub latency_us: LatencyStats,
    pub errors: u64,
}

#[derive(Serialize)]
pub struct LatencyStats {
    pub p50: f64,
    pub p99: f64,
    pub p999: f64,
    pub min: f64,
    pub max: f64,
    pub avg: f64,
}

impl BenchResult {
    #[allow(clippy::too_many_arguments)]
    pub fn from_histogram(
        hist: &Histogram<u64>,
        mode: &str,
        transport: &str,
        connections: usize,
        threads: usize,
        duration_secs: u64,
        payload_bytes: usize,
        errors: u64,
    ) -> Self {
        let total = hist.len();
        let throughput = if duration_secs > 0 {
            total as f64 / duration_secs as f64
        } else {
            0.0
        };
        Self {
            mode: mode.to_string(),
            transport: transport.to_string(),
            connections,
            threads,
            duration_secs,
            payload_bytes,
            total_requests: total,
            throughput_rps: throughput,
            latency_us: LatencyStats {
                p50: hist.value_at_quantile(0.50) as f64,
                p99: hist.value_at_quantile(0.99) as f64,
                p999: hist.value_at_quantile(0.999) as f64,
                min: hist.min() as f64,
                max: hist.max() as f64,
                avg: hist.mean(),
            },
            errors,
        }
    }

    pub fn print_text(&self) {
        println!("=== RDMA Benchmark Results ===");
        println!("Mode:         {}", self.mode);
        println!("Transport:    {}", self.transport);
        println!("Connections:  {}", self.connections);
        println!("Threads:      {}", self.threads);
        println!("Duration:     {}s", self.duration_secs);
        println!("Payload:      {} bytes", self.payload_bytes);
        println!();
        println!("Throughput:   {:.0} req/s", self.throughput_rps);
        println!("Latency:");
        println!("  p50:        {:.1} µs", self.latency_us.p50);
        println!("  p99:        {:.1} µs", self.latency_us.p99);
        println!("  p999:       {:.1} µs", self.latency_us.p999);
        println!("  min:        {:.1} µs", self.latency_us.min);
        println!("  max:        {:.1} µs", self.latency_us.max);
        println!("  avg:        {:.1} µs", self.latency_us.avg);
        println!("Errors:       {}", self.errors);
    }

    pub fn print_json(&self) {
        println!(
            "{}",
            serde_json::to_string_pretty(self).expect("serialize JSON")
        );
    }
}
