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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::clock::Clock;

/// The target every refused-request line has.
pub const TARGET: &str = "spoolframe::refused";

/// How many refusals of one kind from one source are logged a window.
pub const PER_WINDOW: u32 = 10;

/// The window [`PER_WINDOW`] counts in.
pub const WINDOW: Duration = Duration::from_secs(60);

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
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::NoSession => "no_session",
            Kind::Unauthenticated => "unauthenticated",
            Kind::Forbidden => "forbidden",
            Kind::RecentSignInRequired => "recent_sign_in_required",
            Kind::ProfileLocked => "profile_locked",
            Kind::SignInFailed => "sign_in_failed",
            Kind::RateLimited => "rate_limited",
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

/// Which refusals are logged: the first [`PER_WINDOW`] per source and
/// kind in each [`WINDOW`].
#[derive(Debug, Default)]
pub struct Refusals {
    windows: Mutex<Windows>,
    clock: Clock,
}

impl Refusals {
    /// Windows timed by `clock`.
    pub fn new(clock: Clock) -> Self {
        Self {
            windows: Mutex::default(),
            clock,
        }
    }

    /// Whether a refusal of `kind` from `source` is to be logged now. The
    /// summaries of windows that have ended are logged first. Cheap: the
    /// caller looks up what the line names only when this says yes.
    pub fn admit(&self, source: &str, kind: Kind) -> bool {
        let now = self.clock.now();
        let mut summaries = Vec::new();
        let admitted = {
            let mut windows = self.windows.lock();
            windows.sweep(now, &mut summaries);
            windows.admit(source, kind, now, &mut summaries)
        };
        for summary in summaries {
            summary.log();
        }
        admitted
    }

    /// Log `refusal` if [`Refusals::admit`] lets it in.
    pub fn refuse(&self, refusal: &Refusal) {
        if self.admit(&refusal.source, refusal.kind) {
            log(refusal);
        }
    }
}

impl Windows {
    /// Close every window that has ended, at most once a [`WINDOW`]: what
    /// keeps a source that stopped from holding a key.
    fn sweep(&mut self, now: Instant, summaries: &mut Vec<Summary>) {
        if self
            .swept
            .is_some_and(|swept| now.duration_since(swept) < WINDOW)
        {
            return;
        }
        self.swept = Some(now);
        self.by_key.retain(|(source, kind), window| {
            let ended = now.duration_since(window.began) >= WINDOW;
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
        if now.duration_since(window.began) >= WINDOW {
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
        if window.logged < PER_WINDOW {
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
            .filter(|_| refusals.admit(source, Kind::Forbidden))
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

    #[test]
    fn a_source_gets_its_share_a_window_and_others_theirs() {
        let refusals = Refusals::default();
        assert_eq!(admitted(&refusals, "a", 100), PER_WINDOW as usize);
        assert_eq!(admitted(&refusals, "b", 100), PER_WINDOW as usize);
        assert!(refusals.admit("a", Kind::SignInFailed));
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
            refusals.admit(&i.to_string(), Kind::Forbidden);
        }
        let fresh = (0..1000)
            .filter(|i| refusals.admit(&format!("new-{i}"), Kind::Forbidden))
            .count();
        assert_eq!(fresh, PER_WINDOW as usize);
        assert!(refusals.windows.lock().by_key.len() <= MAX_KEYS + 1);
    }

    #[test]
    fn ended_windows_are_swept_away() {
        let clock = Clock::default();
        let refusals = Refusals::new(clock.clone());
        for i in 0..100 {
            refusals.admit(&i.to_string(), Kind::Forbidden);
        }
        clock.advance(WINDOW);
        refusals.admit("late", Kind::Forbidden);
        assert_eq!(refusals.windows.lock().by_key.len(), 1);
    }
}
