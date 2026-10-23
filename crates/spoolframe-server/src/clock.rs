//! The time that expiry, throttle and cache rules judge by (PX-09).
//!
//! The server reads the system clock through a [`Clock`], as it did
//! before. A test moves its clock forward with `Clock::advance` instead
//! of sleeping: each deadline judged by that clock passes at once, and
//! nothing else changes. Timers the runtime runs (`tokio::time`) are not
//! this clock's; they still wait in real time.
//!
//! In this crate's own tests a clock does not follow the system's at all:
//! it is the system's time when it was made, moved only by
//! `Clock::advance`. Two reads in a test are the same time unless the test
//! moved it, so a test never sees a second tick between them. A server an
//! integration test runs (`test-support`) keeps the system's time: what it
//! orders by time (a read after a connection opened) happens as it does in
//! use.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The system clock, plus however far a test has moved it on. Clones share
/// the same time, so one advance moves every part given the clock.
#[derive(Debug, Clone)]
pub struct Clock {
    /// How far a test has moved this clock past its base, in nanoseconds.
    /// Always 0 outside tests: only [`Clock::advance`] changes it.
    ahead: Arc<AtomicU64>,
    /// In this crate's tests, the time it was made: what it reads, plus
    /// `ahead`. `None` follows the system's clock.
    base: Option<(Instant, SystemTime)>,
}

impl Default for Clock {
    fn default() -> Self {
        let base = cfg!(test).then(|| (Instant::now(), SystemTime::now()));
        Self {
            ahead: Arc::default(),
            base,
        }
    }
}

impl Clock {
    /// Now, as an [`Instant`].
    pub fn now(&self) -> Instant {
        self.base.map_or_else(Instant::now, |(at, _)| at) + self.ahead()
    }

    /// Seconds since the Unix epoch: how the database stores times.
    pub fn unix_secs(&self) -> i64 {
        (self.base.map_or_else(SystemTime::now, |(_, at)| at) + self.ahead())
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

    /// In a test, a clock is the time it was made until the test moves it:
    /// two reads never differ by a tick of the system's clock.
    #[test]
    fn a_test_s_clock_moves_only_when_advanced() {
        let before = Instant::now();
        let clock = Clock::default();
        let (instant, secs) = (clock.now(), clock.unix_secs());
        assert!(instant >= before && instant <= Instant::now());
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!((clock.now(), clock.unix_secs()), (instant, secs));
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.unix_secs(), secs + 1);
    }
}
