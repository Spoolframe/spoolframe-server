//! How fast the link to a server carries media (P6-06).
//!
//! Direct play fetches several blocks at once, each timed alone, so a
//! fetch's own rate is a share of the link's, not the link's: four in
//! flight on a 20 Mbit/s link each see about 5. The meter counts instead
//! the bytes of every direct read over the time any was in flight, the
//! link's busy time, as they arrive: on a slow link an 8 MiB block takes
//! long enough that waiting for it to finish would tell too late. Idle
//! time, while the read-ahead is full, is not counted: the link was not
//! asked to carry anything then. Converted reads are not counted: the
//! server holds them while the encode catches up, which says nothing
//! about the link.
//!
//! One meter per server, shared by every playback on it, so a later
//! playback starts from what an earlier one measured.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the rate looks.
pub(crate) const WINDOW: Duration = Duration::from_secs(10);
/// How long a rate is remembered once nothing is fetched: a playback
/// started this long after the last one still starts from it.
pub(crate) const REMEMBER: Duration = Duration::from_secs(10 * 60);
/// The fewest bytes a rate is worked out from: a header read or two is
/// all round trip, and says nothing about the link's speed.
pub(crate) const MIN_BYTES: u64 = 4 * 1024 * 1024;

/// The link to one server's measured rate. Its readings are on
/// [`crate::Session::link_rate`] and a reader's status.
#[derive(Debug, Default)]
pub struct LinkMeter(Mutex<Meter>);

#[derive(Debug, Default)]
struct Meter {
    in_flight: usize,
    /// The busy period now: since when, and what arrived in it.
    open: Option<Busy>,
    /// Busy periods over, newest last.
    closed: VecDeque<Busy>,
    /// The last rate worked out, and when.
    last: Option<(Instant, f64)>,
}

#[derive(Debug, Clone, Copy)]
struct Busy {
    since: Instant,
    /// When bytes last arrived in it, or a read in it ended: its end so far.
    until: Instant,
    bytes: u64,
}

impl Busy {
    fn time(&self) -> Duration {
        self.until.duration_since(self.since)
    }
}

impl LinkMeter {
    /// A read started at `now`.
    pub(crate) fn begin(&self, now: Instant) {
        let mut meter = self.0.lock();
        if meter.in_flight == 0 {
            meter.open = Some(Busy {
                since: now,
                until: now,
                bytes: 0,
            });
        }
        meter.in_flight += 1;
    }

    /// `bytes` of a read arrived at `now`.
    pub(crate) fn received(&self, now: Instant, bytes: u64) {
        let mut meter = self.0.lock();
        if let Some(open) = meter.open.as_mut() {
            open.until = now;
            open.bytes += bytes;
        }
        meter.remember(now);
    }

    /// A read ended at `now`, whether all of it arrived or not.
    pub(crate) fn end(&self, now: Instant) {
        let mut meter = self.0.lock();
        meter.in_flight = meter.in_flight.saturating_sub(1);
        if let Some(open) = meter.open.as_mut() {
            open.until = now;
        }
        if meter.in_flight == 0
            && let Some(over) = meter.open.take()
        {
            meter.closed.push_back(over);
        }
        while meter
            .closed
            .front()
            .is_some_and(|b| now.duration_since(b.until) > WINDOW)
        {
            meter.closed.pop_front();
        }
        meter.remember(now);
    }

    /// The link's rate over the last [`WINDOW`], bytes per second; `None`
    /// until enough was fetched in it.
    #[cfg_attr(not(feature = "media-readers"), allow(dead_code))]
    pub(crate) fn rate(&self, now: Instant) -> Option<f64> {
        self.0.lock().rate(now)
    }

    /// [`Self::rate`], or the last one worked out if it is no older than
    /// [`REMEMBER`]: what a playback starts from.
    pub(crate) fn recent(&self, now: Instant) -> Option<f64> {
        let meter = self.0.lock();
        meter.rate(now).or_else(|| {
            meter
                .last
                .filter(|(at, _)| now.duration_since(*at) <= REMEMBER)
                .map(|(_, rate)| rate)
        })
    }
}

impl Meter {
    fn remember(&mut self, now: Instant) {
        if let Some(rate) = self.rate(now) {
            self.last = Some((now, rate));
        }
    }

    fn rate(&self, now: Instant) -> Option<f64> {
        let (mut bytes, mut time) = (0u64, Duration::ZERO);
        for busy in self.closed.iter().chain(self.open.iter()) {
            if now.duration_since(busy.until) <= WINDOW {
                bytes += busy.bytes;
                time += busy.time();
            }
        }
        (bytes >= MIN_BYTES && !time.is_zero()).then(|| bytes as f64 / time.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn fetches_in_flight_together_count_as_one_link() {
        // Four 8 MiB fetches over the same second: 32 MiB/s, where each
        // alone saw 8.
        let (meter, t) = (LinkMeter::default(), Instant::now());
        for _ in 0..4 {
            meter.begin(t);
        }
        for _ in 0..4 {
            meter.received(at(t, 1000), 8 * MIB);
            meter.end(at(t, 1000));
        }
        assert_eq!(meter.rate(at(t, 1000)), Some(32.0 * MIB as f64));
    }

    #[test]
    fn idle_time_between_fetches_is_not_counted() {
        // 8 MiB in a second, a 5 s pause with the read-ahead full, then
        // 8 MiB in another second: 8 MiB/s, not 16 over 7 s.
        let (meter, t) = (LinkMeter::default(), Instant::now());
        meter.begin(t);
        meter.received(at(t, 1000), 8 * MIB);
        meter.end(at(t, 1000));
        meter.begin(at(t, 6000));
        meter.received(at(t, 7000), 8 * MIB);
        meter.end(at(t, 7000));
        assert_eq!(meter.rate(at(t, 7000)), Some(8.0 * MIB as f64));
    }

    #[test]
    fn a_few_small_reads_are_no_rate() {
        let (meter, t) = (LinkMeter::default(), Instant::now());
        meter.begin(t);
        meter.received(at(t, 50), 64 * 1024);
        meter.end(at(t, 50));
        assert_eq!(meter.rate(at(t, 50)), None);
    }

    #[test]
    fn what_is_older_than_the_window_is_left_out() {
        // A fast burst, then a slow one 20 s later: only the slow counts.
        let (meter, t) = (LinkMeter::default(), Instant::now());
        meter.begin(t);
        meter.received(at(t, 100), 8 * MIB);
        meter.end(at(t, 100));
        meter.begin(at(t, 20_000));
        meter.received(at(t, 24_000), 8 * MIB);
        meter.end(at(t, 24_000));
        assert_eq!(meter.rate(at(t, 24_000)), Some(2.0 * MIB as f64));
    }

    #[test]
    fn a_rate_is_remembered_after_the_window_until_it_is_too_old() {
        let (meter, t) = (LinkMeter::default(), Instant::now());
        meter.begin(t);
        meter.received(at(t, 1000), 8 * MIB);
        meter.end(at(t, 1000));
        let later = at(t, 1000) + Duration::from_secs(60);
        assert_eq!(meter.rate(later), None);
        assert_eq!(meter.recent(later), Some(8.0 * MIB as f64));
        let too_late = at(t, 1000) + REMEMBER + Duration::from_secs(1);
        assert_eq!(meter.recent(too_late), None);
    }

    #[test]
    fn a_failed_fetch_ends_its_time_without_bytes() {
        // One fetch carried 8 MiB in a second; another failed after two.
        let (meter, t) = (LinkMeter::default(), Instant::now());
        meter.begin(t);
        meter.begin(t);
        meter.received(at(t, 1000), 8 * MIB);
        meter.end(at(t, 1000));
        meter.received(at(t, 2000), 0);
        meter.end(at(t, 2000));
        assert_eq!(meter.rate(at(t, 2000)), Some(4.0 * MIB as f64));
    }
}
