//! Pure watched thresholds and marker interpretation.
use spoolframe_types::{Marker, MarkerKind, PlaybackState};
use std::time::Duration;

/// The rules, with defaults; `watch { … }` in the config overrides them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WatchRules {
    /// Under this, an item is not in progress.
    pub started_after: Duration,
    /// Watched at this share of the duration, for an item with no credits
    /// that run to its end.
    pub watched_fraction: f64,
    /// At most one progress write per principal and item this often.
    pub throttle: Duration,
    /// Shows with activity this recent get a next-up episode.
    pub next_up_window: Duration,
    /// How long a next-up row is reused before it is walked again.
    pub next_up_cache: Duration,
}

impl Default for WatchRules {
    fn default() -> Self {
        Self {
            started_after: Duration::from_secs(60),
            watched_fraction: 0.95,
            throttle: Duration::from_secs(10),
            next_up_window: Duration::from_secs(30 * 24 * 3600),
            next_up_cache: Duration::from_secs(60),
        }
    }
}

impl WatchRules {
    /// Public watch state: completed items have no resume position, and
    /// positions before the started threshold stay out of Continue watching.
    pub fn playback_state(&self, position: Option<Duration>, watched: bool) -> PlaybackState {
        PlaybackState {
            position: position.filter(|p| *p >= self.started_after && !watched),
            watched,
            last_viewed_at: None,
            play_count: 0,
            last_played_at: None,
        }
    }

    /// Whether `position` into an item of `duration` counts as watched.
    /// `credits` is where the credits that run to its end start, when the
    /// catalog knows (P2-10): then that is the point, exactly, and the
    /// share of the duration does not apply.
    pub fn is_watched(
        &self,
        position: Duration,
        duration: Option<Duration>,
        credits: Option<Duration>,
    ) -> bool {
        if let Some(credits) = credits {
            return position >= credits;
        }
        let Some(duration) = duration.filter(|d| !d.is_zero()) else {
            return false;
        };
        position.as_secs_f64() >= duration.as_secs_f64() * self.watched_fraction
    }
}

/// Where the credits that run to the end of the item start, if `markers`
/// has such credits.
pub fn credits_to_end(markers: &[Marker]) -> Option<Duration> {
    markers
        .iter()
        .filter(|m| m.kind == MarkerKind::Credits && m.to_end)
        .map(|m| m.start)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rewinding_below_started_removes_the_resume_position() {
        let rules = WatchRules::default();
        assert_eq!(
            rules.playback_state(Some(Duration::from_secs(59)), false),
            PlaybackState::default()
        );
        assert_eq!(
            rules
                .playback_state(Some(Duration::from_secs(60)), false)
                .position,
            Some(Duration::from_secs(60))
        );
    }
    #[test]
    fn a_short_clip_can_be_watched_before_the_started_threshold() {
        let rules = WatchRules::default();
        let position = Duration::from_secs(29);
        let watched = rules.is_watched(position, Some(Duration::from_secs(30)), None);
        assert_eq!(
            rules.playback_state(Some(position), watched),
            PlaybackState {
                position: None,
                watched: true,
                last_viewed_at: None,
                play_count: 0,
                last_played_at: None,
            }
        );
    }
}
