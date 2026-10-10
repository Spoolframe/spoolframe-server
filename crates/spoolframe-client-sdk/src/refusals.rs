//! Refusals on this device in the log, as the server logs its own
//! (finding 46): one WARN line on [`TARGET`] per refusal, naming what was
//! asked, from where, for whom if known, and why; never a code, key or
//! credential. The first is a guest's pairing refused under the device's
//! policy or for a wrong code (P7-03).
//!
//! Whoever can reach the device can make a refused attempt, so the lines
//! are limited as the server's are: per source and reason, [`PER_WINDOW`]
//! in each [`WINDOW`], then one line with how many more there were. At
//! most [`MAX_KEYS`] sources are told apart; past them, new ones share
//! one count.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// The target every refusal line has: the server's.
pub(crate) const TARGET: &str = "spoolframe::refused";

/// How many refusals of one reason from one source are logged a window.
pub(crate) const PER_WINDOW: u32 = 10;

/// The window [`PER_WINDOW`] counts in.
pub(crate) const WINDOW: Duration = Duration::from_secs(60);

/// The most sources counted apart. A device sees few; a flood shares one
/// count past this.
pub(crate) const MAX_KEYS: usize = 1024;

/// The source every source past [`MAX_KEYS`] is counted as.
const OTHERS: &str = "(others)";

/// One refusal, as logged.
#[derive(Debug, Clone)]
pub(crate) struct Refusal<'a> {
    /// What was refused: `start_pairing`, `pair_hello`.
    pub request: &'static str,
    /// Who asked: a peer's short endpoint id, or `local` for this
    /// device's own user.
    pub source: &'a str,
    /// The portable identity it was for, if known.
    pub user: Option<String>,
    pub reason: &'static str,
}

#[derive(Debug)]
struct Window {
    began: Instant,
    logged: u32,
    suppressed: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Refusals {
    windows: Mutex<HashMap<(String, &'static str), Window>>,
}

impl Refusals {
    /// Log `refusal`, within the limits. Whether it was written.
    pub(crate) fn refused(&self, refusal: Refusal<'_>) -> bool {
        self.refused_at(refusal, Instant::now())
    }

    fn refused_at(&self, refusal: Refusal<'_>, now: Instant) -> bool {
        let mut windows = self.windows.lock();
        if windows.len() >= MAX_KEYS {
            windows.retain(|_, w| now.duration_since(w.began) < WINDOW);
        }
        let mut source = refusal.source.to_owned();
        if windows.len() >= MAX_KEYS && !windows.contains_key(&(source.clone(), refusal.reason)) {
            source = OTHERS.to_owned();
        }
        let window = windows
            .entry((source.clone(), refusal.reason))
            .or_insert(Window {
                began: now,
                logged: 0,
                suppressed: 0,
            });
        if now.duration_since(window.began) >= WINDOW {
            if window.suppressed > 0 {
                tracing::warn!(
                    target: TARGET,
                    kind = "external_sign_in_refused",
                    source,
                    reason = refusal.reason,
                    suppressed = window.suppressed,
                    "{} more refusals suppressed",
                    window.suppressed
                );
            }
            *window = Window {
                began: now,
                logged: 0,
                suppressed: 0,
            };
        }
        if window.logged >= PER_WINDOW {
            window.suppressed += 1;
            return false;
        }
        window.logged += 1;
        tracing::warn!(
            target: TARGET,
            kind = "external_sign_in_refused",
            channel = "pair",
            request = refusal.request,
            source,
            user = refusal.user.as_deref().unwrap_or("-"),
            reason = refusal.reason,
            "refused"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(source: &str) -> Refusal<'_> {
        Refusal {
            request: "pair_hello",
            source,
            user: None,
            reason: "wrong code",
        }
    }

    #[test]
    fn a_flood_from_one_source_is_cut_to_the_window_s_lines() {
        let refusals = Refusals::default();
        let t = Instant::now();
        let written = (0..25)
            .filter(|_| refusals.refused_at(refusal("abc"), t))
            .count();
        assert_eq!(written, PER_WINDOW as usize);
        // Another source is counted apart.
        assert!(refusals.refused_at(refusal("def"), t));
        // The next window writes again.
        assert!(refusals.refused_at(refusal("abc"), t + WINDOW));
    }

    #[test]
    fn sources_past_the_limit_share_one_count() {
        let refusals = Refusals::default();
        let t = Instant::now();
        for n in 0..MAX_KEYS {
            assert!(refusals.refused_at(refusal(&n.to_string()), t));
        }
        let written = (0..50)
            .filter(|n| refusals.refused_at(refusal(&format!("new-{n}")), t))
            .count();
        assert_eq!(written, PER_WINDOW as usize);
        assert_eq!(refusals.windows.lock().len(), MAX_KEYS + 1);
    }
}
