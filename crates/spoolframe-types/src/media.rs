//! What a client learns about a media file before it plays it: its length,
//! what it is (container and picture, to show), and the tracks it can
//! choose between (ARCHITECTURE §6).
//!
//! Audio and subtitle codecs stay on the server, where the playback
//! planner uses them; a client chooses an audio language, not a codec.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::codec::{Container, HdrFormat, VideoCodec};
use crate::duration_ms;

/// The tracks of one media file. A track is identified by its position in
/// its list, which is stable for as long as the file does not change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTracks {
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    /// The file's container, from the server's own probe of it. `None`
    /// only where nothing was probed.
    pub container: Option<Container>,
    /// The file's average bits per second, every stream counted, for
    /// "42 Mbit/s": what the container says, else its size over its
    /// length. `None` where the server knows neither.
    ///
    /// This and `VideoTrack::bit_rate` were added without a
    /// PROTOCOL_VERSION bump, so they are not `required::option`: a
    /// server from before them leaves the keys out, and that reads as
    /// `None`.
    pub bit_rate: Option<u64>,
    /// The main picture; `None` for audio only (music).
    pub video: Option<VideoTrack>,
    pub audio: Vec<AudioTrack>,
    pub subtitles: Vec<SubtitleTrack>,
}

/// A file's picture as it is stored, for "4K · HEVC · Dolby Vision ·
/// 10-bit": what the file is, not how it will play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoTrack {
    pub codec: VideoCodec,
    /// Pixels. A 2.40:1 4K film is 3840 by 1600: go by the width for
    /// its class.
    pub width: u32,
    pub height: u32,
    /// Bits per sample: 8, 10 or 12.
    pub bit_depth: u32,
    /// The HDR format; `None` is SDR. Dolby Vision over an HDR10 base is
    /// Dolby Vision. HDR10+ is HDR10 here: its metadata is in the frames,
    /// which the probe does not read.
    pub hdr: Option<HdrFormat>,
    /// The picture's own average bits per second, where the file says:
    /// MP4 does for each stream, Matroska where its muxer wrote
    /// statistics tags (mkvmerge does).
    pub bit_rate: Option<u64>,
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
    fn a_track_s_role_left_out_is_refused() {
        let audio = AudioTrack {
            language: None,
            title: None,
            channels: None,
            default: false,
            role: AudioRole::Commentary,
        };
        crate::test_support::assert_each_required(&audio, &["role"]);
        let subtitle = SubtitleTrack {
            language: None,
            title: None,
            forced: false,
            default: false,
            external: false,
            role: SubtitleRole::Sdh,
        };
        crate::test_support::assert_each_required(&subtitle, &["role"]);
    }

    #[test]
    fn tracks_round_trip_with_the_duration_in_milliseconds() {
        let tracks = MediaTracks {
            duration: Some(Duration::from_millis(6_106_791)),
            container: Some(Container::Matroska),
            bit_rate: Some(42_000_000),
            video: Some(VideoTrack {
                codec: VideoCodec::Hevc,
                width: 3840,
                height: 1600,
                bit_depth: 10,
                hdr: Some(HdrFormat::DolbyVision),
                bit_rate: Some(38_500_000),
            }),
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
    fn tracks_from_a_server_without_bit_rates_still_decode() {
        let tracks = MediaTracks {
            bit_rate: Some(42_000_000),
            video: Some(VideoTrack {
                codec: VideoCodec::Hevc,
                width: 3840,
                height: 2160,
                bit_depth: 10,
                hdr: None,
                bit_rate: Some(38_500_000),
            }),
            ..MediaTracks::default()
        };
        let mut json = serde_json::to_value(&tracks).unwrap();
        assert_eq!(json["bit_rate"], 42_000_000);
        json.as_object_mut().unwrap().remove("bit_rate");
        json["video"].as_object_mut().unwrap().remove("bit_rate");
        let older: MediaTracks = serde_json::from_value(json).unwrap();
        assert_eq!(older.bit_rate, None);
        assert_eq!(older.video.unwrap().bit_rate, None);
    }

    #[test]
    fn a_role_from_a_newer_peer_still_decodes() {
        // A role this build does not know. (A track with no role at all is
        // refused: `a_track_s_role_left_out_is_refused`.)
        let audio: AudioRole = serde_json::from_value(serde_json::json!("karaoke")).unwrap();
        assert_eq!(audio, AudioRole::Other);
        let subtitle: SubtitleRole = serde_json::from_value(serde_json::json!("lyrics")).unwrap();
        assert_eq!(subtitle, SubtitleRole::Full);
    }
}
