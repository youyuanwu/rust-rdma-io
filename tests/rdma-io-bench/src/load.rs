//! Open-loop constant-rate load scheduling for the bench clients.
//!
//! The default bench client is *closed-loop*: it keeps `--in-flight` requests
//! outstanding per connection and runs flat-out at saturation. For the
//! loaded-latency and matched-throughput comparisons we instead need an
//! *open-loop* generator that issues requests at a fixed target rate independent
//! of completion arrival, and measures latency from each request's **scheduled**
//! issue time so a client that falls behind does not hide tail latency
//! (coordinated-omission mitigation).
//!
//! [`RateSchedule`] is a pure, runtime-free helper: given a per-connection rate
//! and a start instant it yields the scheduled [`Instant`] of request `seq`
//! (`start + seq / rate`). Callers gate issuing on these instants with
//! `tokio::time::sleep_until` (non-busy) and record the scheduled instant, not
//! the actual send time. The per-connection rate is the run's total
//! `--target-rps` divided across the connections.

use std::time::{Duration, Instant};

/// Constant-interval schedule of request issue times for one connection.
///
/// Request `seq` (0-based) is scheduled at `start + seq * (1 / rate)`. The
/// schedule is independent of when requests are actually issued, so when a
/// connection falls behind, [`RateSchedule::issue`] still returns the original
/// (past) scheduled instant and latency measured against it reflects the queuing
/// delay rather than hiding it.
#[derive(Clone, Debug)]
pub struct RateSchedule {
    start: Instant,
    secs_per_req: f64,
    next_seq: u64,
}

impl RateSchedule {
    /// Build a schedule issuing `rate_per_sec` requests/second from `start`.
    ///
    /// A non-positive (or non-finite) rate yields a schedule that is never due
    /// (infinite spacing), so callers treat it as "no requests to issue".
    pub fn new(rate_per_sec: f64, start: Instant) -> Self {
        let secs_per_req = if rate_per_sec.is_finite() && rate_per_sec > 0.0 {
            1.0 / rate_per_sec
        } else {
            f64::INFINITY
        };
        Self {
            start,
            secs_per_req,
            next_seq: 0,
        }
    }

    /// Scheduled issue instant of request `seq` (`start + seq * 1/rate`).
    ///
    /// For an infinite/never-due spacing this returns a far-future sentinel.
    pub fn scheduled(&self, seq: u64) -> Instant {
        let offset = self.secs_per_req * seq as f64;
        if offset.is_finite() {
            self.start + Duration::from_secs_f64(offset)
        } else {
            // Never-due sentinel: far enough in the future that callers always
            // prefer any real wake deadline (e.g. the benchmark end).
            self.start + Duration::from_secs(u64::from(u32::MAX))
        }
    }

    /// Scheduled instant of the next request that has not yet been issued.
    pub fn next_scheduled(&self) -> Instant {
        self.scheduled(self.next_seq)
    }

    /// Number of requests issued so far.
    pub fn issued(&self) -> u64 {
        self.next_seq
    }

    /// Mark the next request as issued and return its scheduled instant.
    ///
    /// The returned instant (not the wall-clock send time) is what callers
    /// record for latency, preserving tail fidelity when behind schedule.
    pub fn issue(&mut self) -> Instant {
        let t = self.next_scheduled();
        self.next_seq += 1;
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_is_monotonic_at_the_interval() {
        let start = Instant::now();
        let s = RateSchedule::new(1000.0, start); // 1 ms spacing
        assert_eq!(s.scheduled(0), start);
        let d1 = s.scheduled(1).duration_since(start);
        assert!((d1.as_secs_f64() - 0.001).abs() < 1e-9, "got {d1:?}");
        let d1000 = s.scheduled(1000).duration_since(start);
        assert!((d1000.as_secs_f64() - 1.0).abs() < 1e-6, "got {d1000:?}");
        // strictly increasing
        assert!(s.scheduled(5) > s.scheduled(4));
    }

    #[test]
    fn issue_advances_and_returns_scheduled_time() {
        let start = Instant::now();
        let mut s = RateSchedule::new(2000.0, start); // 0.5 ms spacing
        let t0 = s.issue();
        let t1 = s.issue();
        assert_eq!(t0, start);
        assert_eq!(s.issued(), 2);
        assert!(t1 > t0);
        assert!((t1.duration_since(t0).as_secs_f64() - 0.0005).abs() < 1e-9);
        // next_scheduled tracks the not-yet-issued request
        assert_eq!(s.next_scheduled(), s.scheduled(2));
    }

    #[test]
    fn approx_rate_times_duration_ticks_over_a_window() {
        let start = Instant::now();
        let s = RateSchedule::new(500.0, start);
        // How many requests are scheduled within a 2 s window?
        let window_end = start + Duration::from_secs(2);
        let mut n = 0u64;
        while s.scheduled(n) < window_end {
            n += 1;
        }
        // 500 rps * 2 s = ~1000 (seq 0..=999 land in [start, start+2s)).
        assert!((999..=1000).contains(&n), "got {n}");
    }

    #[test]
    fn behind_schedule_times_stay_in_the_past_not_clamped_to_now() {
        // Start 10 s ago: early requests are overdue but keep their spacing.
        let start = Instant::now() - Duration::from_secs(10);
        let s = RateSchedule::new(100.0, start);
        // request 500 scheduled at start + 5 s => still 5 s in the past.
        assert!(s.scheduled(500) < Instant::now());
        let gap = s.scheduled(101).duration_since(s.scheduled(100));
        assert!((gap.as_secs_f64() - 0.01).abs() < 1e-9);
    }

    #[test]
    fn zero_rate_is_never_due() {
        let start = Instant::now();
        let s = RateSchedule::new(0.0, start);
        assert!(s.next_scheduled() > Instant::now() + Duration::from_secs(3600));
    }
}
