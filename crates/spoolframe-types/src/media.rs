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
    // compat(v1): required at the protocol reset
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
    // compat(v1): required at the protocol reset
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
    /// How the track is timed (PX-03): an external text track's sync and
    /// offset, the same for every user. Sent only to a client that listed
    /// the `spoolframe:subtitle-sync` feature, and only for an external
    /// text track (SRT, WebVTT, ASS); `None` for every other track, whose
    /// timing is the file's.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<SubtitleTiming>,
}

/// How an external text subtitle is timed (PX-03): synced to the file or
/// as it came, and moved by a manual offset on top. Set for everyone who
/// plays the item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleTiming {
    pub sync: SubtitleSync,
    /// Added to every cue, ms, on top of the synced or original timing:
    /// positive shows the text later. What the track is served with
    /// already has it: a client does not add it again.
    pub offset_ms: i64,
    /// Who set the offset last, by display name; `None` while no one has.
    #[serde(deserialize_with = "crate::required::option")]
    pub offset_by: Option<String>,
    /// When, Unix seconds.
    #[serde(deserialize_with = "crate::required::option")]
    pub offset_at: Option<i64>,
}

/// Where a subtitle's sync is (PX-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubtitleSync {
    /// Its own timing, as the file has it: never synced, or put back
    /// with "Use original timing".
    Original,
    /// Being synced, or waiting its turn; it is served as it was until
    /// the sync is done.
    Syncing,
    /// Synced, and served synced.
    Synced {
        /// The shift of most of its cues, ms.
        offset_ms: i64,
        /// The frame-rate ratio its times were multiplied by, in
        /// millionths: 1 000 000 for none, 1 042 709 for 23.976 to 25.
        factor_ppm: u32,
        /// Where scenes were cut or added: the places its shift changes.
        splits: u32,
        /// How well it fell on the file, 0 to 100.
        score: u8,
    },
    /// A sync scored too low to trust: it may be another film's subtitle.
    /// Served with its original timing.
    Uncertain { score: u8 },
    /// The sync could not be made; served as it was.
    Failed { reason: String },
    /// A state this build does not know: show the track as it is.
    #[serde(other)]
    Unknown,
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
            timing: None,
        };
        crate::test_support::assert_each_required(&subtitle, &["role"]);
    }

    fn sidecar(timing: Option<SubtitleTiming>) -> SubtitleTrack {
        SubtitleTrack {
            language: Some("eng".into()),
            title: None,
            forced: false,
            default: false,
            external: true,
            role: SubtitleRole::Full,
            timing,
        }
    }

    /// PX-03: a track's timing is left out when there is none, so a
    /// client from before it reads the track as it did; one sent reads
    /// back as it was, its state by name.
    #[test]
    fn a_track_s_timing_is_sent_only_when_there_is_one() {
        let json = serde_json::to_value(sidecar(None)).unwrap();
        assert!(json.get("timing").is_none(), "{json}");
        let timing = SubtitleTiming {
            sync: SubtitleSync::Synced {
                offset_ms: -3_200,
                factor_ppm: 1_042_709,
                splits: 1,
                score: 87,
            },
            offset_ms: 1_500,
            offset_by: Some("Alice".into()),
            offset_at: Some(1_760_000_000),
        };
        let json = serde_json::to_value(sidecar(Some(timing.clone()))).unwrap();
        assert_eq!(json["timing"]["sync"]["state"], "synced");
        assert_eq!(
            serde_json::from_value::<SubtitleTrack>(json).unwrap(),
            sidecar(Some(timing))
        );
    }

    #[test]
    fn a_sync_state_this_build_does_not_know_is_unknown() {
        let sync: SubtitleSync =
            serde_json::from_value(serde_json::json!({ "state": "transcribing" })).unwrap();
        assert_eq!(sync, SubtitleSync::Unknown);
    }

    #[test]
    fn a_timing_s_offset_author_left_out_is_refused() {
        let timing = SubtitleTiming {
            sync: SubtitleSync::Original,
            offset_ms: 0,
            offset_by: None,
            offset_at: None,
        };
        crate::test_support::assert_each_required(&timing, &["offset_by", "offset_at"]);
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
                timing: None,
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
