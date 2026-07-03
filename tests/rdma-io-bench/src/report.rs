use hdrhistogram::Histogram;
use serde::Serialize;

#[derive(Serialize)]
pub struct BenchResult {
    pub mode: String,
    pub transport: String,
    pub connections: usize,
    pub threads: usize,
    /// Requests kept outstanding per connection (1 for the request/response
    /// tonic modes; the sweep axis for `--mode echo`).
    pub in_flight: usize,
    pub duration_secs: u64,
    pub payload_bytes: usize,
    pub total_requests: u64,
    pub throughput_rps: f64,
    pub latency_us: LatencyStats,
    pub errors: u64,
    /// Client-side user+system CPU seconds consumed during the measured window
    /// (all threads). Only populated by `--mode echo`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<f64>,
    /// CPU microseconds spent per request (`cpu_seconds / total_requests`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_us_per_op: Option<f64>,
    /// Peak resident set size in kilobytes (process lifetime high-water mark).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_rss_kb: Option<u64>,
}

#[derive(Serialize)]
pub struct LatencyStats {
    pub p50: f64,
    pub p95: f64,
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
        in_flight: usize,
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
            in_flight,
            duration_secs,
            payload_bytes,
            total_requests: total,
            throughput_rps: throughput,
            latency_us: LatencyStats {
                p50: hist.value_at_quantile(0.50) as f64,
                p95: hist.value_at_quantile(0.95) as f64,
                p99: hist.value_at_quantile(0.99) as f64,
                p999: hist.value_at_quantile(0.999) as f64,
                min: hist.min() as f64,
                max: hist.max() as f64,
                avg: hist.mean(),
            },
            errors,
            cpu_seconds: None,
            cpu_us_per_op: None,
            peak_rss_kb: None,
        }
    }

    /// Attach client-side resource usage measured over the benchmark window.
    pub fn with_resource_usage(mut self, cpu_seconds: f64, peak_rss_kb: u64) -> Self {
        self.cpu_us_per_op = if self.total_requests > 0 {
            Some(cpu_seconds / self.total_requests as f64 * 1e6)
        } else {
            None
        };
        self.cpu_seconds = Some(cpu_seconds);
        self.peak_rss_kb = Some(peak_rss_kb);
        self
    }

    pub fn print_text(&self) {
        println!("=== RDMA Benchmark Results ===");
        println!("Mode:         {}", self.mode);
        println!("Transport:    {}", self.transport);
        println!("Connections:  {}", self.connections);
        println!("Threads:      {}", self.threads);
        println!("In-flight:    {}", self.in_flight);
        println!("Duration:     {}s", self.duration_secs);
        println!("Payload:      {} bytes", self.payload_bytes);
        println!();
        println!("Throughput:   {:.0} req/s", self.throughput_rps);
        println!("Latency:");
        println!("  p50:        {:.1} µs", self.latency_us.p50);
        println!("  p95:        {:.1} µs", self.latency_us.p95);
        println!("  p99:        {:.1} µs", self.latency_us.p99);
        println!("  p999:       {:.1} µs", self.latency_us.p999);
        println!("  min:        {:.1} µs", self.latency_us.min);
        println!("  max:        {:.1} µs", self.latency_us.max);
        println!("  avg:        {:.1} µs", self.latency_us.avg);
        println!("Errors:       {}", self.errors);
        if let Some(cpu) = self.cpu_seconds {
            println!("CPU time:     {cpu:.2} s");
        }
        if let Some(cpu_op) = self.cpu_us_per_op {
            println!("CPU/op:       {cpu_op:.3} µs");
        }
        if let Some(rss) = self.peak_rss_kb {
            println!("Peak RSS:     {rss} KB");
        }
    }

    pub fn print_json(&self) {
        println!(
            "{}",
            serde_json::to_string_pretty(self).expect("serialize JSON")
        );
    }
}
