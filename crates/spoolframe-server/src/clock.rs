//! The time that expiry, throttle and cache rules judge by (PX-09).
//!
//! The server reads the system clock through a [`Clock`], as it did
//! before. A test moves its clock forward with `Clock::advance` instead
//! of sleeping: each deadline judged by that clock passes at once, and
//! nothing else changes. Timers the runtime runs (`tokio::time`) are not
//! this clock's; they still wait in real time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The system clock, plus however far a test has moved it on. Clones share
/// the same time, so one advance moves every part given the clock.
#[derive(Debug, Clone, Default)]
pub struct Clock {
    /// How far a test has moved this clock past the system's, in
    /// nanoseconds. Always 0 outside tests: only [`Clock::advance`]
    /// changes it.
    ahead: Arc<AtomicU64>,
}

impl Clock {
    /// Now, as an [`Instant`].
    pub fn now(&self) -> Instant {
        Instant::now() + self.ahead()
    }

    /// Seconds since the Unix epoch: how the database stores times.
    pub fn unix_secs(&self) -> i64 {
        (SystemTime::now() + self.ahead())
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn ahead(&self) -> Duration {
        Duration::from_nanos(self.ahead.load(Ordering::Relaxed))
    }

    /// Move this clock, and every clone of it, `by` into the future.
    #[cfg(any(test, feature = "test-support"))]
    pub fn advance(&self, by: Duration) {
        let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        self.ahead.fetch_add(by, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_advance_moves_every_clone_and_both_kinds_of_time() {
        let clock = Clock::default();
        let other = clock.clone();
        let (instant, secs) = (clock.now(), clock.unix_secs());
        other.advance(Duration::from_secs(3600));
        assert!(clock.now() >= instant + Duration::from_secs(3600));
        assert!(clock.unix_secs() >= secs + 3600);
    }

    #[test]
    fn a_clock_never_advanced_is_the_system_s() {
        let clock = Clock::default();
        let before = Instant::now();
        let now = clock.now();
        assert!(now >= before && now <= Instant::now());
    }
}
