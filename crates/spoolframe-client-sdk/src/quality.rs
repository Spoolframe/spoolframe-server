//! The quality picker's choices, and the form a quality is kept in by
//! an app's settings: `auto`, `original`, or bits per second.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::PlaybackQuality;

const CHOICES: [PlaybackQuality; 7] = [
    PlaybackQuality::Auto,
    PlaybackQuality::Bitrate(20_000_000),
    PlaybackQuality::Bitrate(8_000_000),
    PlaybackQuality::Bitrate(4_000_000),
    PlaybackQuality::Bitrate(2_000_000),
    PlaybackQuality::Bitrate(1_000_000),
    PlaybackQuality::Original,
];

impl PlaybackQuality {
    /// The quality picker's choices, in its order: Auto, 20, 8, 4, 2 and
    /// 1 Mbit/s, Original.
    pub fn choices() -> &'static [PlaybackQuality] {
        &CHOICES
    }

    /// This quality's place in [`PlaybackQuality::choices`]: a bitrate
    /// that is not offered (a device's own limit) is Auto's, 0.
    pub fn choice_index(self) -> usize {
        CHOICES.iter().position(|c| *c == self).unwrap_or(0)
    }
}

/// `auto`, `original`, or bits per second.
impl fmt::Display for PlaybackQuality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaybackQuality::Auto => f.write_str("auto"),
            PlaybackQuality::Original => f.write_str("original"),
            PlaybackQuality::Bitrate(bps) => write!(f, "{bps}"),
        }
    }
}

/// A kept quality that is none: not `auto`, `original` or a bitrate
/// above 0.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a playback quality: {0:?}")]
pub struct NotAQuality(pub String);

/// Reads what [`fmt::Display`] writes. `Auto` and `Original`, as a derived
/// form wrote them, read too.
impl FromStr for PlaybackQuality {
    type Err = NotAQuality;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" | "Auto" => Ok(PlaybackQuality::Auto),
            "original" | "Original" => Ok(PlaybackQuality::Original),
            _ => match s.parse::<u64>() {
                Ok(bps) if bps > 0 => Ok(PlaybackQuality::Bitrate(bps)),
                _ => Err(NotAQuality(s.to_owned())),
            },
        }
    }
}

/// As its settings string.
impl Serialize for PlaybackQuality {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// From its settings string, or from what an app kept before: a bare
/// number of bits per second, or a derived `{"Bitrate": n}`.
impl<'de> Deserialize<'de> for PlaybackQuality {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Kept {
            Text(String),
            Bps(u64),
            Derived {
                #[serde(rename = "Bitrate", alias = "bitrate")]
                bitrate: u64,
            },
        }
        let text = match Kept::deserialize(deserializer)? {
            Kept::Text(text) => text,
            Kept::Bps(bps) | Kept::Derived { bitrate: bps } => bps.to_string(),
        };
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choices_run_auto_then_highest_first_then_original() {
        let choices = PlaybackQuality::choices();
        assert_eq!(choices.first(), Some(&PlaybackQuality::Auto));
        assert_eq!(choices[1], PlaybackQuality::Bitrate(20_000_000));
        assert_eq!(choices.last(), Some(&PlaybackQuality::Original));
    }

    #[test]
    fn a_bitrate_not_offered_shows_as_auto() {
        assert_eq!(PlaybackQuality::Bitrate(6_000_000).choice_index(), 0);
        assert_eq!(PlaybackQuality::Bitrate(8_000_000).choice_index(), 2);
        assert_eq!(PlaybackQuality::Original.choice_index(), 6);
    }

    #[test]
    fn every_choice_is_read_back_as_it_was_kept() {
        for choice in PlaybackQuality::choices() {
            assert_eq!(choice.to_string().parse(), Ok(*choice));
            let json = serde_json::to_string(choice).unwrap();
            assert_eq!(
                serde_json::from_str::<PlaybackQuality>(&json).unwrap(),
                *choice
            );
        }
        assert_eq!(PlaybackQuality::Bitrate(8_000_000).to_string(), "8000000");
    }

    #[test]
    fn a_kept_value_that_is_no_choice_is_refused() {
        for kept in ["best", "0", "", "-1"] {
            assert!(kept.parse::<PlaybackQuality>().is_err(), "{kept:?}");
        }
    }

    #[test]
    fn what_an_app_kept_before_reads_too() {
        for (kept, quality) in [
            (r#""Auto""#, PlaybackQuality::Auto),
            (r#""Original""#, PlaybackQuality::Original),
            ("4000000", PlaybackQuality::Bitrate(4_000_000)),
            (
                r#"{"Bitrate":2000000}"#,
                PlaybackQuality::Bitrate(2_000_000),
            ),
        ] {
            assert_eq!(
                serde_json::from_str::<PlaybackQuality>(kept).unwrap(),
                quality,
                "{kept}"
            );
        }
    }
}
