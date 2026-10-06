//! How a viewer wants tracks chosen (P1-20). Kept per user on the server,
//! so a TV and a laptop choose the same.

use serde::{Deserialize, Serialize};

use crate::media::AudioRole;

/// A language code that stands for the item's original language: the
/// language of the file's default audio track.
pub const ORIGINAL_LANGUAGE: &str = "original";

/// Every field is required on the wire (PX-15): a peer that leaves one out
/// is refused, not filled in. The server keeps these as JSON and reads its
/// older rows with the defaults filled in itself (`store::tracks`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackPreferences {
    pub audio: AudioPreferences,
    pub subtitles: SubtitlePreferences,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct SubtitlePreferences {
    pub mode: SubtitleMode,
    /// ISO 639 codes in order of preference. Empty: the audio's language.
    pub languages: Vec<String>,
    pub sdh: SdhPreference,
    /// How picture subtitles (PGS, VobSub, DVB) are shown when the app
    /// leaves it to the server. It has no effect on a server that does not
    /// read them as text.
    pub bitmap: BitmapPreference,
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

/// How a viewer wants picture subtitles shown: read as text by the
/// server, so any device shows them styled and sized, or as the pictures
/// they are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitmapPreference {
    /// Drawn by the device if it can, else burned into the video.
    Pictures,
    /// Read as text when the server can read the track; else as
    /// `Pictures`. Also any value this build does not know. (Last, as
    /// serde wants of `other`.)
    #[default]
    #[serde(other)]
    Text,
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

    /// The defaults as JSON, with `edit` applied.
    fn full(edit: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
        let mut json = serde_json::to_value(TrackPreferences::default()).unwrap();
        edit(&mut json);
        json
    }

    #[test]
    fn picture_subtitles_are_read_as_text_unless_the_viewer_wants_pictures() {
        assert_eq!(
            TrackPreferences::default().subtitles.bitmap,
            BitmapPreference::Text
        );
        let p: TrackPreferences =
            serde_json::from_value(full(|j| j["subtitles"]["bitmap"] = "pictures".into())).unwrap();
        assert_eq!(p.subtitles.bitmap, BitmapPreference::Pictures);
    }

    #[test]
    fn a_picture_subtitle_preference_this_build_does_not_know_reads_as_text() {
        let p: TrackPreferences =
            serde_json::from_value(full(|j| j["subtitles"]["bitmap"] = "hologram".into())).unwrap();
        assert_eq!(p.subtitles.bitmap, BitmapPreference::Text);
    }

    #[test]
    fn every_field_round_trips() {
        let p: TrackPreferences = serde_json::from_value(full(|_| {})).unwrap();
        assert_eq!(p, TrackPreferences::default());
    }

    /// Each field in turn, by its JSON path, left out of the defaults.
    #[test]
    fn preferences_missing_any_field_are_refused() {
        for (section, field) in [
            (None, "audio"),
            (None, "subtitles"),
            (Some("audio"), "languages"),
            (Some("audio"), "avoid"),
            (Some("subtitles"), "mode"),
            (Some("subtitles"), "languages"),
            (Some("subtitles"), "sdh"),
            (Some("subtitles"), "bitmap"),
        ] {
            let json = full(|j| {
                let object = match section {
                    Some(section) => &mut j[section],
                    None => j,
                };
                object.as_object_mut().unwrap().remove(field);
            });
            let error = serde_json::from_value::<TrackPreferences>(json).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("missing field `{field}`")),
                "{section:?}.{field}: {error}"
            );
        }
    }
}
