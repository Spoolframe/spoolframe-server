//! Refused requests in the log (finding 46): a failed sign-in, a session
//! that is not valid where it was used, a consumer calling an admin
//! method, a locked profile, a sign-in that is too old for the change, a
//! rate limit. Each is one line on [`TARGET`], at WARN, naming what was
//! asked, from where, by whom if known, and why; never a password, PIN,
//! code or token.
//!
//! The lines are not [`crate::telemetry::LOCAL_ONLY`]: they hold no secret,
//! and a collector is where an attempt from many places is seen at once.
//!
//! Whoever can make a request can make a refused one, so the lines are
//! limited: per source and kind of refusal, [`PER_WINDOW`] in each
//! [`WINDOW`], then one line with how many more there were. At most
//! [`MAX_KEYS`] sources are told apart; past them, new ones share one
//! count, so a flood from many addresses is limited too.
//!
//! A refused `Hello` ([`HelloRefusal`]) is logged by a limiter of its own,
//! [`Refusals::hellos`]: once per device an hour, at INFO. An app of
//! another protocol revision is refused each time it dials, which may be
//! every few seconds for days; one line an hour is enough to see it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::clock::Clock;
use crate::metrics::Counter;

/// The target every refused-request line has.
pub const TARGET: &str = "spoolframe::refused";

/// How many refusals of one kind from one source are logged a window.
pub const PER_WINDOW: u32 = 10;

/// The window [`PER_WINDOW`] counts in.
pub const WINDOW: Duration = Duration::from_secs(60);

/// The window a device's refused `Hello`s are counted in: one line each.
pub const HELLO_WINDOW: Duration = Duration::from_secs(60 * 60);

/// The most sources counted apart. A key is a short string and three
/// numbers, so this is about a megabyte at worst.
pub const MAX_KEYS: usize = 10_000;

/// The source that every source past [`MAX_KEYS`] is counted as.
const OTHERS: &str = "(others)";

/// What kind of refusal: the `kind` field, and half of what is limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// No session was given where one is needed: what every signed-out
    /// caller does, so it is logged at INFO only.
    NoSession,
    /// A session was given, and it is not live, or not for here.
    Unauthenticated,
    /// Not an admin.
    Forbidden,
    /// The session's sign-in is too old for the change.
    RecentSignInRequired,
    /// The session's profile is locked on this device until its PIN.
    ProfileLocked,
    /// A wrong password, PIN, passkey or code.
    SignInFailed,
    /// Too many failed attempts, or attempts from strangers.
    RateLimited,
    /// A `Hello` refused: another protocol revision, or one that could not
    /// be read ([`HelloRefusal`]). INFO: nothing was tried that needs a
    /// key.
    HelloRefused,
}

impl Kind {
    pub const ALL: [Kind; 8] = [
        Kind::NoSession,
        Kind::Unauthenticated,
        Kind::Forbidden,
        Kind::RecentSignInRequired,
        Kind::ProfileLocked,
        Kind::SignInFailed,
        Kind::RateLimited,
        Kind::HelloRefused,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::NoSession => "no_session",
            Kind::Unauthenticated => "unauthenticated",
            Kind::Forbidden => "forbidden",
            Kind::RecentSignInRequired => "recent_sign_in_required",
            Kind::ProfileLocked => "profile_locked",
            Kind::SignInFailed => "sign_in_failed",
            Kind::RateLimited => "rate_limited",
            Kind::HelloRefused => "hello_refused",
        }
    }
}

/// Where a request came in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// JSON-RPC over the admin API's socket (the web UI and the CLI).
    AdminApi,
    /// The admin listener's `/auth` and other HTTP routes.
    Web,
    /// The client protocol, over QUIC.
    Quic,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::AdminApi, Channel::Web, Channel::Quic];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::AdminApi => "admin_api",
            Channel::Web => "web",
            Channel::Quic => "quic",
        }
    }
}

/// One refusal, as logged.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub kind: Kind,
    pub channel: Channel,
    /// The method, request type or path refused.
    pub request: String,
    /// What the caller is known by: an address, or a device's short id.
    pub source: String,
    /// The username, if known: the session's user, or the one a sign-in
    /// gave (which need not exist).
    pub user: Option<String>,
    pub reason: String,
}

/// One source's refusals of one kind in the current window.
#[derive(Debug)]
struct Window {
    began: Instant,
    logged: u32,
    suppressed: u64,
}

#[derive(Debug, Default)]
struct Windows {
    by_key: HashMap<(String, Kind), Window>,
    /// When every window was last looked at.
    swept: Option<Instant>,
}

/// A window that ended with refusals left out of the log.
struct Summary {
    source: String,
    kind: Kind,
    suppressed: u64,
}

/// How many refusals of one kind from one source are logged, and in how
/// long a window.
#[derive(Debug, Clone, Copy)]
struct Limit {
    per_window: u32,
    window: Duration,
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            per_window: PER_WINDOW,
            window: WINDOW,
        }
    }
}

/// Which refusals are logged: the first [`PER_WINDOW`] per source and
/// kind in each [`WINDOW`].
#[derive(Debug, Default)]
pub struct Refusals {
    windows: Mutex<Windows>,
    clock: Clock,
    limit: Limit,
    /// Every refusal, logged or not, by [`Kind::ALL`] and
    /// [`Channel::ALL`]: for the metrics (P3-14).
    counts: [[Counter; 3]; 8],
}

impl Refusals {
    /// Windows timed by `clock`.
    pub fn new(clock: Clock) -> Self {
        Self {
            windows: Mutex::default(),
            clock,
            limit: Limit::default(),
            counts: Default::default(),
        }
    }

    /// The limiter of refused `Hello`s: one per source in each
    /// [`HELLO_WINDOW`], timed by `clock`, with [`MAX_KEYS`] as ever.
    pub fn hellos(clock: Clock) -> Self {
        Self {
            windows: Mutex::default(),
            clock,
            limit: Limit {
                per_window: 1,
                window: HELLO_WINDOW,
            },
            counts: Default::default(),
        }
    }

    /// Whether a refusal of `kind` from `source` on `channel` is to be
    /// logged now; it is counted either way. The summaries of windows that
    /// have ended are logged first. Cheap: the caller looks up what the
    /// line names only when this says yes.
    pub fn admit(&self, source: &str, kind: Kind, channel: Channel) -> bool {
        self.counts[kind as usize][channel as usize].inc();
        let now = self.clock.now();
        let mut summaries = Vec::new();
        let admitted = {
            let mut windows = self.windows.lock();
            windows.sweep(now, self.limit, &mut summaries);
            windows.admit(source, kind, now, self.limit, &mut summaries)
        };
        for summary in summaries {
            summary.log();
        }
        admitted
    }

    /// Every refusal so far, logged or not, by kind and channel: every
    /// pair, 0 for those not seen.
    pub fn counts(&self) -> impl Iterator<Item = (Kind, Channel, u64)> + '_ {
        Kind::ALL.into_iter().flat_map(move |kind| {
            Channel::ALL.into_iter().map(move |channel| {
                (
                    kind,
                    channel,
                    self.counts[kind as usize][channel as usize].get(),
                )
            })
        })
    }

    /// Log `refusal` if [`Refusals::admit`] lets it in.
    pub fn refuse(&self, refusal: &Refusal) {
        if self.admit(&refusal.source, refusal.kind, refusal.channel) {
            log(refusal);
        }
    }
}

impl Windows {
    /// Close every window that has ended, at most once a window: what
    /// keeps a source that stopped from holding a key.
    fn sweep(&mut self, now: Instant, limit: Limit, summaries: &mut Vec<Summary>) {
        if self
            .swept
            .is_some_and(|swept| now.duration_since(swept) < limit.window)
        {
            return;
        }
        self.swept = Some(now);
        self.by_key.retain(|(source, kind), window| {
            let ended = now.duration_since(window.began) >= limit.window;
            if ended && window.suppressed > 0 {
                summaries.push(Summary {
                    source: source.clone(),
                    kind: *kind,
                    suppressed: window.suppressed,
                });
            }
            !ended
        });
    }

    fn admit(
        &mut self,
        source: &str,
        kind: Kind,
        now: Instant,
        limit: Limit,
        summaries: &mut Vec<Summary>,
    ) -> bool {
        let mut key = (source.to_owned(), kind);
        if !self.by_key.contains_key(&key) && self.by_key.len() >= MAX_KEYS {
            key.0 = OTHERS.to_owned();
        }
        let counted_as = key.0.clone();
        let window = self.by_key.entry(key).or_insert(Window {
            began: now,
            logged: 0,
            suppressed: 0,
        });
        if now.duration_since(window.began) >= limit.window {
            if window.suppressed > 0 {
                summaries.push(Summary {
                    source: counted_as,
                    kind,
                    suppressed: window.suppressed,
                });
            }
            *window = Window {
                began: now,
                logged: 0,
                suppressed: 0,
            };
        }
        if window.logged < limit.per_window {
            window.logged += 1;
            true
        } else {
            window.suppressed += 1;
            false
        }
    }
}

impl Summary {
    fn log(&self) {
        if self.kind == Kind::HelloRefused {
            tracing::info!(
                target: TARGET,
                kind = self.kind.as_str(),
                source = %self.source,
                suppressed = self.suppressed,
                "{} more refusals suppressed",
                self.suppressed
            );
        } else {
            tracing::warn!(
                target: TARGET,
                kind = self.kind.as_str(),
                source = %self.source,
                suppressed = self.suppressed,
                "{} more refusals suppressed",
                self.suppressed
            );
        }
    }
}

/// One refused `Hello`, as logged: what the device said of itself, as far
/// as it could be read, and what the server speaks.
#[derive(Debug, Clone)]
pub struct HelloRefusal {
    /// The device's endpoint, short.
    pub source: String,
    /// The device's name, if someone signed in from it: the server's own
    /// record, never what this `Hello` says.
    pub device: Option<String>,
    /// The protocol revision the `Hello` said, if it could be read.
    pub client_protocol: Option<u32>,
    /// Its `client_name` and `client_version`, bounded, if they could be
    /// read.
    pub client: Option<String>,
    pub server_protocol: u32,
    /// This server's version.
    pub server_version: String,
    pub reason: String,
}

/// The refused-`Hello` line, unlimited: the caller has had it admitted by
/// [`Refusals::hellos`].
pub fn log_hello(refusal: &HelloRefusal) {
    let HelloRefusal {
        source,
        device,
        client_protocol,
        client,
        server_protocol,
        server_version,
        reason,
    } = refusal;
    tracing::info!(
        target: TARGET,
        kind = Kind::HelloRefused.as_str(),
        channel = Channel::Quic.as_str(),
        request = "hello",
        source = %source,
        device = device.as_deref(),
        client_protocol = *client_protocol,
        client = client.as_deref(),
        server_protocol = *server_protocol,
        server_version = %server_version,
        reason = %reason,
        "hello refused"
    );
}

/// The line itself, unlimited: the caller has had it admitted.
pub fn log(refusal: &Refusal) {
    let Refusal {
        kind,
        channel,
        request,
        source,
        user,
        reason,
    } = refusal;
    if *kind == Kind::NoSession {
        tracing::info!(
            target: TARGET,
            kind = kind.as_str(),
            channel = channel.as_str(),
            request = %request,
            source = %source,
            user = user.as_deref(),
            reason = %reason,
            "request refused"
        );
    } else {
        tracing::warn!(
            target: TARGET,
            kind = kind.as_str(),
            channel = channel.as_str(),
            request = %request,
            source = %source,
            user = user.as_deref(),
            reason = %reason,
            "request refused"
        );
    }
}

/// At most 64 characters of what a caller chose (a request type, a
/// username), so a line stays a line: a control character in it is
/// written escaped (`\n`, `\u{1b}`), or a newline in a username would
/// write a line of its own into the text log (finding 59).
pub fn bounded(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        if c.is_control() {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
        if escaped.chars().count() >= 64 {
            break;
        }
    }
    escaped.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted(refusals: &Refusals, source: &str, n: usize) -> usize {
        (0..n)
            .filter(|_| refusals.admit(source, Kind::Forbidden, Channel::Quic))
            .count()
    }

    #[test]
    fn a_username_with_a_newline_stays_on_one_line() {
        let username = "alice\n2026-10-07T12:00:00Z WARN request refused user=admin\r\n";
        let logged = bounded(&username.repeat(2));
        assert!(
            !logged.chars().any(char::is_control),
            "{logged:?} carries a control character"
        );
        assert!(logged.starts_with(r"alice\n2026"), "{logged:?}");
        // Escaped, it is still at most 64 characters.
        assert_eq!(logged.chars().count(), 64);
    }

    /// Every refusal is counted by kind and channel, those left out of the
    /// log too.
    #[test]
    fn every_refusal_is_counted_logged_or_not() {
        let refusals = Refusals::default();
        for _ in 0..PER_WINDOW + 5 {
            refusals.admit("a", Kind::SignInFailed, Channel::Web);
        }
        refusals.admit("b", Kind::RateLimited, Channel::Quic);
        let counts: Vec<_> = refusals.counts().filter(|(_, _, n)| *n > 0).collect();
        assert_eq!(
            counts,
            [
                (Kind::SignInFailed, Channel::Web, u64::from(PER_WINDOW) + 5),
                (Kind::RateLimited, Channel::Quic, 1),
            ]
        );
        assert_eq!(refusals.counts().count(), 24);
    }

    #[test]
    fn a_source_gets_its_share_a_window_and_others_theirs() {
        let refusals = Refusals::default();
        assert_eq!(admitted(&refusals, "a", 100), PER_WINDOW as usize);
        assert_eq!(admitted(&refusals, "b", 100), PER_WINDOW as usize);
        assert!(refusals.admit("a", Kind::SignInFailed, Channel::Quic));
    }

    #[test]
    fn a_new_window_admits_again() {
        let clock = Clock::default();
        let refusals = Refusals::new(clock.clone());
        admitted(&refusals, "a", 100);
        clock.advance(WINDOW);
        assert_eq!(admitted(&refusals, "a", 100), PER_WINDOW as usize);
    }

    #[test]
    fn sources_past_the_cap_share_one_count() {
        let refusals = Refusals::default();
        for i in 0..MAX_KEYS {
            refusals.admit(&i.to_string(), Kind::Forbidden, Channel::Quic);
        }
        let fresh = (0..1000)
            .filter(|i| refusals.admit(&format!("new-{i}"), Kind::Forbidden, Channel::Quic))
            .count();
        assert_eq!(fresh, PER_WINDOW as usize);
        assert!(refusals.windows.lock().by_key.len() <= MAX_KEYS + 1);
    }

    #[test]
    fn a_refused_hello_is_admitted_once_an_hour_per_source() {
        let clock = Clock::default();
        let hellos = Refusals::hellos(clock.clone());
        assert!(hellos.admit("phone", Kind::HelloRefused, Channel::Quic));
        assert!(!hellos.admit("phone", Kind::HelloRefused, Channel::Quic));
        assert!(hellos.admit("tablet", Kind::HelloRefused, Channel::Quic));
        clock.advance(HELLO_WINDOW - Duration::from_secs(1));
        assert!(!hellos.admit("phone", Kind::HelloRefused, Channel::Quic));
        clock.advance(Duration::from_secs(1));
        assert!(hellos.admit("phone", Kind::HelloRefused, Channel::Quic));
    }

    #[test]
    fn refused_hellos_from_endpoints_past_the_cap_share_one_line() {
        let hellos = Refusals::hellos(Clock::default());
        for i in 0..MAX_KEYS {
            hellos.admit(&i.to_string(), Kind::HelloRefused, Channel::Quic);
        }
        let fresh = (0..1000)
            .filter(|i| hellos.admit(&format!("new-{i}"), Kind::HelloRefused, Channel::Quic))
            .count();
        assert_eq!(fresh, 1);
        assert!(hellos.windows.lock().by_key.len() <= MAX_KEYS + 1);
    }

    #[test]
    fn ended_windows_are_swept_away() {
        let clock = Clock::default();
        let refusals = Refusals::new(clock.clone());
        for i in 0..100 {
            refusals.admit(&i.to_string(), Kind::Forbidden, Channel::Quic);
        }
        clock.advance(WINDOW);
        refusals.admit("late", Kind::Forbidden, Channel::Quic);
        assert_eq!(refusals.windows.lock().by_key.len(), 1);
    }
}
