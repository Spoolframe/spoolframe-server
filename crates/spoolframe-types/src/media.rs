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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// An ISO 639-2 code such as `eng`, when the file says.
    pub language: Option<String>,
    pub title: Option<String>,
    /// Only for foreign-language parts of the dialogue; shown even when
    /// subtitles are off.
    pub forced: bool,
    pub default: bool,
    /// From a separate file next to the media, not inside it.
    pub external: bool,
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
            }],
            subtitles: vec![SubtitleTrack {
                language: Some("eng".into()),
                title: Some("Forced".into()),
                forced: true,
                default: false,
                external: true,
            }],
        };
        let json = serde_json::to_value(&tracks).unwrap();
        assert_eq!(json["duration"], 6_106_791);
        assert_eq!(serde_json::from_value::<MediaTracks>(json).unwrap(), tracks);
    }
}
