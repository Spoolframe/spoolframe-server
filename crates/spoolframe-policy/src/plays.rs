//! What counts as a play (PX-52): a pure rule over one play's progress,
//! next to [`crate::watch`]'s watched rule, which it builds on for video.
//!
//! A play that never meets the rule is still kept, uncounted: it is
//! history ("started, stopped"), not a play.

use std::time::Duration;

use crate::watch::WatchRules;

/// A track counts once it has played this long, however long it is.
pub const TRACK_ENOUGH: Duration = Duration::from_secs(4 * 60);

/// Which rule a play is judged by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayKind {
    /// A film or an episode.
    Video,
    Track,
}

/// One play as far as it has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayProgress {
    /// Time actually played: what the position moved by while playing,
    /// never more than the time that passed. A seek adds nothing.
    pub played: Duration,
    /// Where in the item this play started (a resume starts past 0).
    pub from: Duration,
    /// Where the play is now.
    pub position: Duration,
    /// The item's length, when known.
    pub duration: Option<Duration>,
    /// Where the credits that run to the end start, when the catalog
    /// knows ([`crate::watch::credits_to_end`]).
    pub credits: Option<Duration>,
}

/// Whether `play` counts.
///
/// - A track counts once played for its bar, half its length or
///   [`TRACK_ENOUGH`], whichever is less (Last.fm's rule; of unknown
///   length, [`TRACK_ENOUGH`]), by a play that started before the bar. A
///   track stopped past its bar and resumed in a new playback has counted
///   once already: the resumed play, starting at or past the bar, does
///   not count again, however long it then plays.
/// - A film or an episode counts when it is watched, by the same rule as
///   its watch state ([`WatchRules::is_watched`]: its credits, or the
///   watched share of its length without them), by a play that started
///   before that point and played something, and at least half of the
///   stretch this play covered, from where it started to where it is,
///   was played. So a seek to the end is not a play, nor is a viewing
///   paused in the credits and resumed in a new playback a second one,
///   while one that skips an intro, or finishes a film started on
///   another evening, is.
pub fn counts(rules: &WatchRules, kind: PlayKind, play: &PlayProgress) -> bool {
    match kind {
        PlayKind::Track => {
            let bar = play
                .duration
                .filter(|d| !d.is_zero())
                .map_or(TRACK_ENOUGH, |d| (d / 2).min(TRACK_ENOUGH));
            play.from < bar && play.played >= bar
        }
        PlayKind::Video => {
            let covered = play.position.saturating_sub(play.from);
            !play.played.is_zero()
                && !rules.is_watched(play.from, play.duration, play.credits)
                && rules.is_watched(play.position, play.duration, play.credits)
                && play.played * 2 >= covered
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);
    const SEC: Duration = Duration::from_secs(1);

    fn track(played: Duration, duration: Option<Duration>) -> bool {
        let play = PlayProgress {
            played,
            from: Duration::ZERO,
            position: played,
            duration,
            credits: None,
        };
        counts(&WatchRules::default(), PlayKind::Track, &play)
    }

    fn video(
        from: Duration,
        position: Duration,
        played: Duration,
        credits: Option<Duration>,
    ) -> bool {
        let play = PlayProgress {
            played,
            from,
            position,
            duration: Some(100 * MIN),
            credits,
        };
        counts(&WatchRules::default(), PlayKind::Video, &play)
    }

    #[test]
    fn a_track_counts_at_half_its_length() {
        assert!(track(3 * MIN, Some(5 * MIN)));
        assert!(track(150 * Duration::from_secs(1), Some(5 * MIN)));
        assert!(!track(149 * Duration::from_secs(1), Some(5 * MIN)));
        assert!(!track(MIN, Some(5 * MIN)));
    }

    #[test]
    fn a_long_track_counts_at_four_minutes() {
        assert!(track(4 * MIN, Some(12 * MIN)));
        assert!(!track(4 * MIN - Duration::from_secs(1), Some(12 * MIN)));
    }

    #[test]
    fn a_track_of_unknown_length_counts_at_four_minutes() {
        assert!(track(4 * MIN, None));
        assert!(track(4 * MIN, Some(Duration::ZERO)));
        assert!(!track(3 * MIN, None));
    }

    #[test]
    fn a_film_counts_at_the_watched_share_without_credits() {
        assert!(video(Duration::ZERO, 95 * MIN, 95 * MIN, None));
        assert!(!video(Duration::ZERO, 94 * MIN, 94 * MIN, None));
    }

    #[test]
    fn a_film_with_credits_counts_at_them() {
        let credits = Some(90 * MIN);
        assert!(video(Duration::ZERO, 90 * MIN, 90 * MIN, credits));
        assert!(!video(Duration::ZERO, 89 * MIN, 89 * MIN, credits));
    }

    #[test]
    fn a_seek_to_the_end_is_not_a_play() {
        assert!(!video(
            Duration::ZERO,
            96 * MIN,
            10 * Duration::from_secs(1),
            None
        ));
        assert!(!video(Duration::ZERO, 96 * MIN, 47 * MIN, None));
    }

    fn resumed_track(from: Duration, played: Duration, duration: Duration) -> bool {
        let play = PlayProgress {
            played,
            from,
            position: from + played,
            duration: Some(duration),
            credits: None,
        };
        counts(&WatchRules::default(), PlayKind::Track, &play)
    }

    /// A 10-minute track stopped at 4:00 counted then; its resume at
    /// 4:00, in a new playback, is not a second play.
    #[test]
    fn a_track_resumed_at_or_past_its_bar_does_not_count_again() {
        assert!(!resumed_track(4 * MIN, 6 * MIN, 10 * MIN));
        assert!(!resumed_track(5 * MIN, 5 * MIN, 10 * MIN));
        // Half of a 5-minute track is its bar.
        assert!(!resumed_track(150 * SEC, 150 * SEC, 5 * MIN));
    }

    #[test]
    fn a_track_resumed_before_its_bar_counts_once_it_has_played_it() {
        assert!(resumed_track(3 * MIN, 4 * MIN, 10 * MIN));
        assert!(!resumed_track(3 * MIN, 4 * MIN - SEC, 10 * MIN));
    }

    /// Paused in the credits, resumed in a new playback: the resumed play
    /// starts past the watched point, and is not a second viewing.
    #[test]
    fn a_film_resumed_in_its_credits_does_not_count_again() {
        let credits = Some(90 * MIN);
        assert!(!video(92 * MIN, 99 * MIN, 7 * MIN, credits));
        assert!(!video(90 * MIN, 99 * MIN, 9 * MIN, credits));
        assert!(!video(96 * MIN, 99 * MIN, 3 * MIN, None));
    }

    /// Started at the last minute, nothing played: no play.
    #[test]
    fn a_film_with_nothing_played_does_not_count() {
        assert!(!video(96 * MIN, 96 * MIN, Duration::ZERO, None));
        assert!(!video(94 * MIN, 95 * MIN, Duration::ZERO, None));
    }

    #[test]
    fn a_film_resumed_just_before_its_credits_counts() {
        assert!(video(89 * MIN, 91 * MIN, 2 * MIN, Some(90 * MIN)));
    }

    #[test]
    fn a_skipped_intro_or_a_film_finished_another_evening_counts() {
        assert!(video(Duration::ZERO, 96 * MIN, 93 * MIN, None));
        assert!(video(60 * MIN, 96 * MIN, 36 * MIN, None));
        assert!(!video(60 * MIN, 96 * MIN, 17 * MIN, None));
    }
}
