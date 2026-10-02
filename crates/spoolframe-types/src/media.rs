//! What a client learns about a media file before it plays it: its length
//! and the tracks it can choose between (ARCHITECTURE §6).
//!
//! Codec details stay on the server, where the playback planner uses them;
//! a client chooses an audio language, not a codec.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::duration_ms;

/// The tracks of one media file. A track is identified by its position in
/// its list, which is stable for as long as the file does not change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTracks {
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    pub audio: Vec<AudioTrack>,
    pub subtitles: Vec<SubtitleTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrack {
    /// An ISO 639-2 code such as `eng`, when the file says.
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u32>,
    /// The file marks it as the one to play by default.
    pub default: bool,
    /// What the track is for, whatever its title says (P1-20).
    #[serde(default)]
    pub role: AudioRole,
}

/// What an audio track is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioRole {
    /// The programme's own sound.
    #[default]
    Main,
    /// Narration of what is on screen, for blind and partially sighted
    /// viewers.
    AudioDescription,
    Commentary,
    /// Anything else (karaoke, music and effects only), and any role this
    /// build does not know.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// An ISO 639-2 code such as `eng`, when the file says.
    pub language: Option<String>,
    pub title: Option<String>,
    /// Only for foreign-language parts of the dialogue; shown even when
    /// subtitles are off. The same as `role == Forced`.
    pub forced: bool,
    pub default: bool,
    /// From a separate file next to the media, not inside it.
    pub external: bool,
    /// What the track is for, whatever its title says (P1-20).
    #[serde(default)]
    pub role: SubtitleRole,
}

/// What a subtitle track is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleRole {
    /// For deaf and hard-of-hearing viewers: dialogue plus sounds and
    /// speakers.
    Sdh,
    /// Only foreign-language parts and signs.
    Forced,
    Commentary,
    /// All the dialogue. Also any role this build does not know: showing
    /// it is the safe guess. (Last, as serde wants of `other`.)
    #[default]
    #[serde(other)]
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_round_trip_with_the_duration_in_milliseconds() {
        let tracks = MediaTracks {
            duration: Some(Duration::from_millis(6_106_791)),
            audio: vec![AudioTrack {
                language: Some("eng".into()),
                title: None,
                channels: Some(6),
                default: true,
                role: AudioRole::Main,
            }],
            subtitles: vec![SubtitleTrack {
                language: Some("eng".into()),
                title: Some("Forced".into()),
                forced: true,
                default: false,
                external: true,
                role: SubtitleRole::Forced,
            }],
        };
        let json = serde_json::to_value(&tracks).unwrap();
        assert_eq!(json["duration"], 6_106_791);
        assert_eq!(serde_json::from_value::<MediaTracks>(json).unwrap(), tracks);
    }

    #[test]
    fn roles_from_an_older_or_newer_peer_still_decode() {
        // Older: no role at all.
        let old: AudioTrack = serde_json::from_value(serde_json::json!({
            "language": "eng", "title": null, "channels": 2, "default": false
        }))
        .unwrap();
        assert_eq!(old.role, AudioRole::Main);
        // Newer: a role this build does not know.
        let audio: AudioRole = serde_json::from_value(serde_json::json!("karaoke")).unwrap();
        assert_eq!(audio, AudioRole::Other);
        let subtitle: SubtitleRole = serde_json::from_value(serde_json::json!("lyrics")).unwrap();
        assert_eq!(subtitle, SubtitleRole::Full);
    }
}
