//! The seek gate: while a seek is in flight the player still says where
//! it was, so the progress (and a controller's markers) go by the seek's
//! target instead, until a new position lands within [`SEEK_LANDED`] of
//! it or the app says it is done. A seek that never lands there (cut
//! short at the file's end, snapped to a keyframe further off, refused)
//! counts no more once the player has played on [`SEEK_LANDED`] from
//! where it did land, or after [`SEEK_GIVE_UP`].

use std::time::Duration;
use tokio::time::Instant;

/// A position this close to a seek's target is where the seek landed.
pub const SEEK_LANDED: Duration = Duration::from_secs(2);

/// How long a seek that has not landed holds the markers and progress at
/// its target, at most.
pub const SEEK_GIVE_UP: Duration = Duration::from_secs(5);

/// A seek in flight.
#[derive(Clone, Copy)]
struct Seek {
    target: Duration,
    began: Instant,
    /// The player's last position before it: the player saying it again
    /// has not moved yet, so it lands nowhere.
    from: Option<Duration>,
    /// The first position since that is not `from`: where it went.
    went: Option<Duration>,
}

/// One player's seek gate ([`crate::ManagedPlayback::seek_started`], a
/// local file's controller).
#[derive(Default)]
pub(crate) struct SeekGate {
    seeking: Option<Seek>,
    /// The player's last position, as given.
    last: Option<Duration>,
}

impl SeekGate {
    /// The player seeks to `target`: its positions are the old place's
    /// until it lands.
    pub(crate) fn started(&mut self, target: Duration) {
        self.seeking = Some(Seek {
            target,
            began: Instant::now(),
            from: self.last,
            went: None,
        });
    }

    /// The player says its seek is done: its positions count again.
    pub(crate) fn done(&mut self) {
        self.seeking = None;
    }

    /// The player at `at`: where it is for the markers and the progress,
    /// and whether that is settled. While a seek is in flight it is the
    /// target, not settled. A seek is over once a new position lands
    /// within [`SEEK_LANDED`] of its target, or plays on [`SEEK_LANDED`]
    /// past where the seek went, or after [`SEEK_GIVE_UP`].
    pub(crate) fn position(&mut self, at: Duration) -> (Duration, bool) {
        self.last = Some(at);
        let Some(mut seek) = self.seeking else {
            return (at, true);
        };
        let new = seek.from != Some(at);
        if new && seek.went.is_none() {
            seek.went = Some(at);
        }
        let landed = new && at.abs_diff(seek.target) <= SEEK_LANDED;
        let played_on = seek.went.is_some_and(|went| at > went + SEEK_LANDED);
        let gave_up = Instant::now().saturating_duration_since(seek.began) >= SEEK_GIVE_UP;
        if landed || played_on || gave_up {
            self.seeking = None;
            return (at, true);
        }
        self.seeking = Some(seek);
        (seek.target, false)
    }
}
