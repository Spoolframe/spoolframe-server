//! How the link to a playing device carries its media: what the QUIC
//! connection says every [`EVERY`], for the log and for the playback's
//! own totals. For telling why a playback off the LAN stalls (the link,
//! the relay, or a path that will not hold still), which the server
//! otherwise cannot see: it sends as fast as the connection lets it.
//!
//! The arithmetic is here, on plain counters; `client.rs` reads them off
//! the connection.

use std::time::{Duration, Instant};

/// How often a connection with a live playback is looked at.
pub const EVERY: Duration = Duration::from_secs(10);

/// How often a connection's path change is logged at most: the first
/// change at once, then one line for every change since.
pub const PATH_LOG_EVERY: Duration = Duration::from_secs(10);

/// What a connection has carried so far: every path's, closed ones
/// included, as iroh counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub sent_bytes: u64,
    pub sent_datagrams: u64,
    pub received_bytes: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    /// Summed over the paths open now: a closed path's are not kept, so
    /// this can go down, and an interval counts only what it gained.
    pub congestion_events: u64,
    /// Changes of the path packets go by, since the connection opened.
    pub path_switches: u64,
}

/// What the path packets go by now looks like.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PathNow {
    /// `lan …`, `direct …` or `relay …`, as the connection log names it.
    pub name: String,
    pub kind: PathKind,
    pub rtt: Duration,
    /// The congestion window, bytes.
    pub cwnd: u64,
    pub mtu: u16,
    /// Paths open at once (iroh's multipath).
    pub open: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PathKind {
    Lan,
    Direct,
    Relay,
    #[default]
    Unknown,
}

impl PathKind {
    /// What a span's `connection.path` says.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Direct => "direct",
            Self::Relay => "relay",
            Self::Unknown => "unknown",
        }
    }

    /// The kind of a path named as [`PathNow::name`] names it.
    pub fn of(name: &str) -> Self {
        match name.split(' ').next() {
            Some("lan") => Self::Lan,
            Some("direct") => Self::Direct,
            Some("relay") => Self::Relay,
            _ => Self::Unknown,
        }
    }
}

/// One interval of a connection: what it carried between two readings.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Interval {
    pub secs: f64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
    pub sent_datagrams: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    pub congestion_events: u64,
    pub path_switches: u64,
}

impl Interval {
    /// From reading `before` to reading `after`, `secs` apart. A counter
    /// that went down (a closed path's congestion events) gives nothing,
    /// not a wrap.
    pub fn between(before: &Counters, after: &Counters, secs: f64) -> Self {
        Self {
            secs,
            sent_bytes: after.sent_bytes.saturating_sub(before.sent_bytes),
            received_bytes: after.received_bytes.saturating_sub(before.received_bytes),
            sent_datagrams: after.sent_datagrams.saturating_sub(before.sent_datagrams),
            lost_packets: after.lost_packets.saturating_sub(before.lost_packets),
            lost_bytes: after.lost_bytes.saturating_sub(before.lost_bytes),
            congestion_events: after
                .congestion_events
                .saturating_sub(before.congestion_events),
            path_switches: after.path_switches.saturating_sub(before.path_switches),
        }
    }

    /// Sent, Mbit/s.
    pub fn send_mbps(&self) -> f64 {
        mbps(self.sent_bytes, self.secs)
    }

    /// Received, Mbit/s.
    pub fn receive_mbps(&self) -> f64 {
        mbps(self.received_bytes, self.secs)
    }

    /// Packets lost, as a share of those sent, in percent.
    pub fn loss_percent(&self) -> f64 {
        percent(self.lost_packets, self.sent_datagrams)
    }
}

fn mbps(bytes: u64, secs: f64) -> f64 {
    if secs > 0.0 {
        bytes as f64 * 8.0 / secs / 1e6
    } else {
        0.0
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// A playback's link, over every interval its device's connections had
/// while it was live: the line logged when it ends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub intervals: u32,
    pub secs: f64,
    pub sent_bytes: u64,
    pub sent_datagrams: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub path_switches: u64,
    pub max_rtt: Duration,
    pub lan_secs: f64,
    pub direct_secs: f64,
    pub relay_secs: f64,
    /// The last interval's: what the playback's detail shows now.
    pub last: Option<(Interval, PathNow)>,
}

impl Totals {
    pub fn add(&mut self, interval: &Interval, path: &PathNow) {
        self.intervals += 1;
        self.secs += interval.secs;
        self.sent_bytes += interval.sent_bytes;
        self.sent_datagrams += interval.sent_datagrams;
        self.lost_packets += interval.lost_packets;
        self.congestion_events += interval.congestion_events;
        self.path_switches += interval.path_switches;
        self.max_rtt = self.max_rtt.max(path.rtt);
        match path.kind {
            PathKind::Lan => self.lan_secs += interval.secs,
            PathKind::Direct => self.direct_secs += interval.secs,
            PathKind::Relay => self.relay_secs += interval.secs,
            PathKind::Unknown => {}
        }
        self.last = Some((*interval, path.clone()));
    }

    /// Sent over the whole playback, Mbit/s.
    pub fn send_mbps(&self) -> f64 {
        mbps(self.sent_bytes, self.secs)
    }

    pub fn loss_percent(&self) -> f64 {
        percent(self.lost_packets, self.sent_datagrams)
    }
}

/// Which path changes of one connection are logged: the first at once,
/// then at most one line a [`PATH_LOG_EVERY`] that counts the changes
/// since. A phone on a cellular network can flip between two of its
/// addresses several times a second, and a line for each buries the log.
#[derive(Debug, Default)]
pub struct PathLog {
    logged_at: Option<Instant>,
    /// Changes not logged since the last line.
    held: u64,
    /// The paths those changes went between, first seen first.
    between: Vec<String>,
}

/// A path change to log, and how many it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLine {
    /// Changes left out since the last line, not counting this one.
    pub held: u64,
    /// The paths they went between.
    pub between: Vec<String>,
}

impl PathLog {
    /// A change to `path` at `now`: a line to log, or `None` while one
    /// was logged under [`PATH_LOG_EVERY`] ago.
    pub fn change(&mut self, path: &str, now: Instant) -> Option<PathLine> {
        if !self.between.iter().any(|p| p == path) && self.between.len() < 4 {
            self.between.push(path.to_owned());
        }
        if self
            .logged_at
            .is_some_and(|at| now.saturating_duration_since(at) < PATH_LOG_EVERY)
        {
            self.held += 1;
            return None;
        }
        self.logged_at = Some(now);
        Some(PathLine {
            held: std::mem::take(&mut self.held),
            between: std::mem::take(&mut self.between),
        })
    }
}

/// Whether a connection punched a hole through NAT, as far as the server
/// can tell from the paths its packets go by: one that starts on a relay
/// is one iroh tries to take direct, and it did once a direct (or LAN)
/// path is selected. One that is direct from the start needed no hole.
/// iroh counts its own attempts only on the side that dials, and only
/// with its `metrics` feature, so the server counts these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HolePunch {
    /// No path selected yet.
    #[default]
    Unknown,
    /// Direct from the start.
    NotNeeded,
    /// On a relay, not direct yet.
    Trying,
    /// On a relay first, direct since.
    Punched,
}

/// What a path change says of a connection's hole punching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punch {
    /// Its first path is a relay: iroh tries to go direct.
    Attempt,
    /// It went direct from a relay.
    Success,
}

impl HolePunch {
    /// The path packets go by is `kind` now: an attempt or a success, the
    /// first time either happens.
    pub fn path(&mut self, kind: PathKind) -> Option<Punch> {
        let direct = matches!(kind, PathKind::Lan | PathKind::Direct);
        match (*self, kind) {
            (Self::Unknown, PathKind::Relay) => {
                *self = Self::Trying;
                Some(Punch::Attempt)
            }
            (Self::Unknown, _) if direct => {
                *self = Self::NotNeeded;
                None
            }
            (Self::Trying, _) if direct => {
                *self = Self::Punched;
                Some(Punch::Success)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_on_a_relay_first_is_an_attempt_and_going_direct_its_success() {
        let mut punch = HolePunch::default();
        assert_eq!(punch.path(PathKind::Unknown), None);
        assert_eq!(punch.path(PathKind::Relay), Some(Punch::Attempt));
        assert_eq!(punch.path(PathKind::Relay), None);
        assert_eq!(punch.path(PathKind::Direct), Some(Punch::Success));
        // Back on the relay and direct again: one success per connection.
        assert_eq!(punch.path(PathKind::Relay), None);
        assert_eq!(punch.path(PathKind::Lan), None);
    }

    #[test]
    fn a_connection_direct_from_the_start_punched_nothing() {
        let mut punch = HolePunch::default();
        assert_eq!(punch.path(PathKind::Lan), None);
        assert_eq!(punch.path(PathKind::Relay), None);
        assert_eq!(punch.path(PathKind::Direct), None);
    }

    fn counters(sent: u64, datagrams: u64, lost: u64, congestion: u64, switches: u64) -> Counters {
        Counters {
            sent_bytes: sent,
            sent_datagrams: datagrams,
            received_bytes: sent / 100,
            lost_packets: lost,
            lost_bytes: lost * 1200,
            congestion_events: congestion,
            path_switches: switches,
        }
    }

    #[test]
    fn an_interval_is_the_difference_and_its_rates() {
        let before = counters(1_000_000, 1_000, 10, 2, 3);
        let after = counters(13_500_000, 11_000, 110, 5, 43);
        let i = Interval::between(&before, &after, 10.0);
        assert_eq!(i.sent_bytes, 12_500_000);
        assert!((i.send_mbps() - 10.0).abs() < 1e-9, "{}", i.send_mbps());
        assert!(
            (i.loss_percent() - 1.0).abs() < 1e-9,
            "{}",
            i.loss_percent()
        );
        assert_eq!(i.congestion_events, 3);
        assert_eq!(i.path_switches, 40);
    }

    #[test]
    fn a_counter_that_went_down_counts_nothing() {
        // A path closed and took its congestion events with it.
        let before = counters(0, 0, 0, 9, 0);
        let after = counters(0, 0, 0, 2, 0);
        assert_eq!(
            Interval::between(&before, &after, 10.0).congestion_events,
            0
        );
    }

    #[test]
    fn nothing_sent_is_no_rate_and_no_loss() {
        let i = Interval::between(&Counters::default(), &Counters::default(), 0.0);
        assert_eq!(i.send_mbps(), 0.0);
        assert_eq!(i.loss_percent(), 0.0);
    }

    #[test]
    fn totals_add_up_and_time_each_kind_of_path() {
        let mut totals = Totals::default();
        let one = Interval {
            secs: 10.0,
            sent_bytes: 10_000_000,
            sent_datagrams: 8_000,
            lost_packets: 80,
            path_switches: 2,
            ..Interval::default()
        };
        let direct = PathNow {
            name: "direct [2607:fb90::1]:51454".into(),
            kind: PathKind::Direct,
            rtt: Duration::from_millis(80),
            ..PathNow::default()
        };
        let relay = PathNow {
            name: "relay https://usw1-1.relay.n0.iroh.link./".into(),
            kind: PathKind::Relay,
            rtt: Duration::from_millis(140),
            ..PathNow::default()
        };
        totals.add(&one, &direct);
        totals.add(&one, &relay);
        assert_eq!(totals.intervals, 2);
        assert_eq!(totals.direct_secs, 10.0);
        assert_eq!(totals.relay_secs, 10.0);
        assert_eq!(totals.max_rtt, Duration::from_millis(140));
        assert_eq!(totals.path_switches, 4);
        assert!((totals.send_mbps() - 8.0).abs() < 1e-9);
        assert!((totals.loss_percent() - 1.0).abs() < 1e-9);
        assert_eq!(totals.last.as_ref().unwrap().1.kind, PathKind::Relay);
    }

    #[test]
    fn a_path_is_known_by_its_name() {
        assert_eq!(PathKind::of("lan 192.168.86.7:61063"), PathKind::Lan);
        assert_eq!(PathKind::of("direct [2607:fb90::1]:1"), PathKind::Direct);
        assert_eq!(PathKind::of("relay https://x./"), PathKind::Relay);
        assert_eq!(PathKind::of("none"), PathKind::Unknown);
    }

    #[test]
    fn the_first_change_is_logged_and_a_flapping_path_once_every_ten_seconds() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut log = PathLog::default();
        let first = log.change("direct A", at(0)).unwrap();
        assert_eq!(first.held, 0);
        // Four flips a second for the next ten seconds: none logged.
        for n in 1..40 {
            let path = if n % 2 == 0 { "direct A" } else { "direct B" };
            assert_eq!(log.change(path, at(n * 250)), None, "{n}");
        }
        let next = log.change("direct B", at(10_000)).unwrap();
        assert_eq!(next.held, 39);
        // The paths since the first line, in the order they came.
        assert_eq!(next.between, ["direct B", "direct A"]);
        // Quiet for a while, then one change: logged at once.
        let late = log.change("relay R", at(60_000)).unwrap();
        assert_eq!(late.held, 0);
        assert_eq!(late.between, ["relay R"]);
    }
}
