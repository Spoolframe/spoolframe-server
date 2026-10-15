//! What one PMS's requests did (P3-14): how many, how many failed, and how
//! long they took, for the server's metrics. Plain atomics: this crate
//! knows nothing of how they are reported.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The upper bounds of the latency buckets, in seconds: a LAN PMS answers
/// in milliseconds, a busy or sleeping one in seconds, and
/// [`super::REQUEST_TIMEOUT`] ends the rest.
pub const LATENCY_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Told how long each request took, as it ends: for a histogram recorded
/// value by value (the OTLP export's) rather than read as buckets.
pub type Observer = Arc<dyn Fn(Duration) + Send + Sync>;

/// Counted for every request to one PMS, whichever view of its catalog
/// made it.
#[derive(Default)]
pub struct RequestStats {
    requests: AtomicU64,
    errors: AtomicU64,
    /// Requests in each bucket alone, not cumulative.
    buckets: [AtomicU64; LATENCY_BUCKETS.len()],
    sum_micros: AtomicU64,
    observer: OnceLock<Observer>,
}

impl std::fmt::Debug for RequestStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestStats")
            .field("counts", &self.counts())
            .finish_non_exhaustive()
    }
}

/// [`RequestStats`] as they are now.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestCounts {
    pub requests: u64,
    /// Requests that failed: unreachable, refused, or an unexpected
    /// status. A 404 is an answer, not a failure.
    pub errors: u64,
    /// Cumulative: requests that took at most each bound, in seconds.
    pub buckets: Vec<(f64, u64)>,
    pub sum_seconds: f64,
}

impl RequestStats {
    pub(crate) fn record(&self, took: Duration, failed: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        let secs = took.as_secs_f64();
        if let Some(i) = LATENCY_BUCKETS.iter().position(|&le| secs <= le) {
            self.buckets[i].fetch_add(1, Ordering::Relaxed);
        }
        self.sum_micros
            .fetch_add(took.as_micros() as u64, Ordering::Relaxed);
        if let Some(observer) = self.observer.get() {
            observer(took);
        }
    }

    /// Tell `observer` of every request from now on. Only the first is
    /// kept: the stats are one PMS's, and so is whoever watches them.
    pub fn observe(&self, observer: Observer) {
        let _ = self.observer.set(observer);
    }

    pub fn counts(&self) -> RequestCounts {
        let mut cumulative = 0;
        let buckets = LATENCY_BUCKETS
            .iter()
            .zip(&self.buckets)
            .map(|(&le, n)| {
                cumulative += n.load(Ordering::Relaxed);
                (le, cumulative)
            })
            .collect();
        RequestCounts {
            requests: self.requests.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            buckets,
            sum_seconds: self.sum_micros.load(Ordering::Relaxed) as f64 / 1e6,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_cumulative_and_failures_counted_apart() {
        let stats = RequestStats::default();
        stats.record(Duration::from_millis(3), false);
        stats.record(Duration::from_millis(300), true);
        stats.record(Duration::from_secs(20), true);
        let counts = stats.counts();
        assert_eq!(counts.requests, 3);
        assert_eq!(counts.errors, 2);
        assert_eq!(counts.buckets[0], (0.005, 1));
        assert_eq!(counts.buckets[6], (0.5, 2));
        // Over the last bound: only in +Inf, which is `requests`.
        assert_eq!(counts.buckets.last(), Some(&(10.0, 2)));
        assert!((counts.sum_seconds - 20.303).abs() < 1e-9);
    }

    #[test]
    fn an_observer_is_told_each_request_s_time() {
        let stats = RequestStats::default();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let told = seen.clone();
        stats.observe(Arc::new(move |took| told.lock().unwrap().push(took)));
        stats.record(Duration::from_millis(3), false);
        stats.record(Duration::from_millis(300), true);
        assert_eq!(
            *seen.lock().unwrap(),
            [Duration::from_millis(3), Duration::from_millis(300)]
        );
    }
}
