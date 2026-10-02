//! How a viewer wants tracks chosen (P1-20). Kept per user on the server,
//! so a TV and a laptop choose the same.

use serde::{Deserialize, Serialize};

use crate::media::AudioRole;

/// A language code that stands for the item's original language: the
/// language of the file's default audio track.
pub const ORIGINAL_LANGUAGE: &str = "original";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackPreferences {
    pub audio: AudioPreferences,
    pub subtitles: SubtitlePreferences,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioPreferences {
    /// ISO 639 codes in order of preference, or [`ORIGINAL_LANGUAGE`].
    /// Empty: the file's default track.
    pub languages: Vec<String>,
    /// Roles never chosen while another track is left.
    pub avoid: Vec<AudioRole>,
}

impl Default for AudioPreferences {
    fn default() -> Self {
        Self {
            languages: Vec::new(),
            avoid: vec![AudioRole::AudioDescription, AudioRole::Commentary],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitlePreferences {
    pub mode: SubtitleMode,
    /// ISO 639 codes in order of preference. Empty: the audio's language.
    pub languages: Vec<String>,
    pub sdh: SdhPreference,
}

/// When subtitles are shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleMode {
    /// Always, in a preferred language.
    Always,
    /// Only when the audio is not in a preferred subtitle language.
    ForeignOnly,
    Off,
    /// Only forced subtitles (foreign parts, signs) in the audio's
    /// language. Also any mode this build does not know. (Last, as serde
    /// wants of `other`.)
    #[default]
    #[serde(other)]
    ForcedOnly,
}

/// What to do with subtitles for the deaf and hard of hearing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdhPreference {
    /// SDH before plain subtitles.
    Prefer,
    /// Never SDH.
    Avoid,
    /// Plain subtitles first, SDH when there are none. Also any value this
    /// build does not know. (Last, as serde wants of `other`.)
    #[default]
    #[serde(other)]
    Accept,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_user_follows_the_file_avoids_description_and_shows_only_forced() {
        let p = TrackPreferences::default();
        assert!(p.audio.languages.is_empty());
        assert_eq!(
            p.audio.avoid,
            [AudioRole::AudioDescription, AudioRole::Commentary]
        );
        assert_eq!(p.subtitles.mode, SubtitleMode::ForcedOnly);
    }

    #[test]
    fn a_partial_audio_section_keeps_the_default_avoid_list() {
        let p: TrackPreferences =
            serde_json::from_value(serde_json::json!({"audio": {"languages": ["eng"]}})).unwrap();
        assert_eq!(p.audio.languages, ["eng"]);
        assert_eq!(p.audio.avoid, AudioPreferences::default().avoid);
    }

    #[test]
    fn missing_fields_take_the_defaults() {
        let p: TrackPreferences =
            serde_json::from_value(serde_json::json!({"subtitles": {"mode": "always"}})).unwrap();
        assert_eq!(p.subtitles.mode, SubtitleMode::Always);
        assert_eq!(p.audio, AudioPreferences::default());
    }
}
