//! How a viewer wants tracks chosen. Kept per user on the server,
//! so a TV and a laptop choose the same.

use serde::{Deserialize, Serialize};

use crate::media::AudioRole;

/// A language code that stands for the item's original language: the
/// language of the file's default audio track.
pub const ORIGINAL_LANGUAGE: &str = "original";

/// Every field is required on the wire: a peer that leaves one out
/// is refused, not filled in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TrackPreferences {
    pub audio: AudioPreferences,
    pub subtitles: SubtitlePreferences,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SubtitlePreferences {
    pub mode: SubtitleMode,
    /// ISO 639 codes in order of preference. Empty: the audio's language.
    pub languages: Vec<String>,
    pub sdh: SdhPreference,
    /// How picture subtitles (PGS, VobSub, DVB) are shown when the app
    /// leaves it to the server. It has no effect on a server that does not
    /// read them as text.
    pub bitmap: BitmapPreference,
    /// Forced subtitles (only the parts in another language) among a
    /// subtitle search's results. Playback follows `mode`.
    pub forced: ForcedPreference,
}

/// When subtitles are shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

/// What to do with subtitles for the deaf and hard of hearing: in
/// playback, which track is chosen; in a subtitle search, the
/// results' order, or which are listed at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum SdhPreference {
    /// SDH before plain subtitles.
    Prefer,
    /// Never SDH: a search lists none.
    Avoid,
    /// Only SDH: no subtitles where there is none, and a search lists no
    /// plain ones. An older peer that does not know it reads it as
    /// `Accept`.
    Only,
    /// Plain subtitles first, SDH when there are none. Also any value this
    /// build does not know. (Last, as serde wants of `other`.)
    #[default]
    #[serde(other)]
    Accept,
}

/// What to do with forced subtitles (only the parts in another language:
/// signs, a foreign line) in a subtitle search's results: the
/// same four ways as [`SdhPreference`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ForcedPreference {
    /// Forced ones before the rest.
    Prefer,
    /// None forced.
    Avoid,
    /// Only forced ones.
    Only,
    /// The rest first, forced ones after. Also any value this build does
    /// not know. (Last, as serde wants of `other`.)
    #[default]
    #[serde(other)]
    Accept,
}

/// How a viewer wants picture subtitles shown: read as text by the
/// server, so any device shows them styled and sized, or as the pictures
/// they are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

    #[test]
    fn only_sdh_reads_and_an_unknown_sdh_preference_is_accept() {
        let p: TrackPreferences =
            serde_json::from_value(full(|j| j["subtitles"]["sdh"] = "only".into())).unwrap();
        assert_eq!(p.subtitles.sdh, SdhPreference::Only);
        let p: TrackPreferences =
            serde_json::from_value(full(|j| j["subtitles"]["sdh"] = "sometimes".into())).unwrap();
        assert_eq!(p.subtitles.sdh, SdhPreference::Accept);
    }

    #[test]
    fn a_forced_preference_reads_by_name_and_an_unknown_one_is_accept() {
        assert_eq!(
            TrackPreferences::default().subtitles.forced,
            ForcedPreference::Accept
        );
        for (name, wanted) in [
            ("prefer", ForcedPreference::Prefer),
            ("avoid", ForcedPreference::Avoid),
            ("only", ForcedPreference::Only),
            ("accept", ForcedPreference::Accept),
            ("hologram", ForcedPreference::Accept),
        ] {
            let p: TrackPreferences =
                serde_json::from_value(full(|j| j["subtitles"]["forced"] = name.into())).unwrap();
            assert_eq!(p.subtitles.forced, wanted, "{name}");
        }
    }

    /// Preferences without the forced preference, as a peer before the
    /// reset to revision 1 could send them, are refused.
    #[test]
    fn preferences_without_the_forced_preference_are_refused() {
        let error = serde_json::from_value::<TrackPreferences>(full(|j| {
            j["subtitles"].as_object_mut().unwrap().remove("forced");
        }))
        .unwrap_err();
        assert!(
            error.to_string().contains("missing field `forced`"),
            "{error}"
        );
    }

    /// Each field in turn, by its JSON path, left out of the defaults.
    /// (Not `subtitles.forced`, which a peer before it leaves out.)
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
