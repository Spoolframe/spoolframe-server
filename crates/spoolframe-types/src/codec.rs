//! The words server and client share for formats (ARCHITECTURE §8).
//!
//! The server knows streams by ffprobe's names (`hevc`, `hdmv_pgs_subtitle`)
//! and a GStreamer client knows its decoders by caps (`video/x-h265`);
//! compared as strings they never match. Each side maps its own names to
//! these enums instead. A format either side does not know is `Unknown`,
//! which no client supports, so a plan says so rather than guessing.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! vocabulary {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident => $wire:literal,)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vdoc])* $variant,)*
            /// A format this build does not know.
            #[serde(other)]
            Unknown,
        }

        impl $name {
            /// The name on the wire.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $wire,)*
                    $name::Unknown => "unknown",
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

vocabulary!(
    /// A video codec.
    VideoCodec {
        H264 => "h264",
        Hevc => "hevc",
        Av1 => "av1",
        Vp9 => "vp9",
        Vp8 => "vp8",
        Mpeg2 => "mpeg2",
        Mpeg4 => "mpeg4",
        Vc1 => "vc1",
    }
);

vocabulary!(
    /// An audio codec. `Dts` covers DTS-HD MA too: the core is DTS.
    AudioCodec {
        Aac => "aac",
        Ac3 => "ac3",
        Eac3 => "eac3",
        Dts => "dts",
        TrueHd => "true_hd",
        Flac => "flac",
        Alac => "alac",
        Opus => "opus",
        Vorbis => "vorbis",
        Mp3 => "mp3",
        Pcm => "pcm",
    }
);

vocabulary!(
    /// A container. `Matroska` covers WebM, which is a subset of it.
    Container {
        Matroska => "matroska",
        Mp4 => "mp4",
        MpegTs => "mpeg_ts",
        Avi => "avi",
        Flac => "flac",
        Mp3 => "mp3",
        Ogg => "ogg",
    }
);

vocabulary!(
    /// A subtitle format.
    SubtitleFormat {
        Srt => "srt",
        Ass => "ass",
        WebVtt => "web_vtt",
        MovText => "mov_text",
        Pgs => "pgs",
        VobSub => "vob_sub",
        DvbSub => "dvb_sub",
    }
);

vocabulary!(
    /// How a client takes media in (P2-01).
    Delivery {
        /// Range reads of the file itself (P1-08).
        Range => "range",
        /// HLS playlists and segments (P2-05).
        Hls => "hls",
        /// One stream written as it is encoded, read by range (P2-04).
        Progressive => "progressive",
    }
);

vocabulary!(
    /// A high dynamic range format.
    HdrFormat {
        Hdr10 => "hdr10",
        Hlg => "hlg",
        DolbyVision => "dolby_vision",
    }
);

/// The most video a client can show. Each limit left out is no limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoLimits {
    /// Lines, e.g. 1080.
    #[serde(default)]
    pub height: Option<u32>,
    /// Frames per second.
    #[serde(default)]
    pub frame_rate: Option<u32>,
    /// Bits per sample, e.g. 8 for a client without 10-bit decoding.
    #[serde(default)]
    pub bit_depth: Option<u32>,
    /// The HDR formats it shows as HDR; anything else is tone-mapped to
    /// SDR. Left out: all of them.
    #[serde(default)]
    pub hdr: Option<BTreeSet<HdrFormat>>,
}

/// What a client can play. Belongs to the endpoint, not to a session: two
/// profiles on one device have the same decoders. A client builds it from
/// what it actually has (the Linux client from the GStreamer registry),
/// not from a fixed list.
///
/// Every field is optional on the wire, so a client that predates one
/// still parses, and gets what it got before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(default)]
    pub video: BTreeSet<VideoCodec>,
    #[serde(default)]
    pub audio: BTreeSet<AudioCodec>,
    #[serde(default)]
    pub containers: BTreeSet<Container>,
    #[serde(default)]
    pub subtitles: BTreeSet<SubtitleFormat>,
    /// How it takes media in, preferred first. Empty means range reads
    /// only, which is what a Phase 1 client does.
    #[serde(default)]
    pub delivery: Vec<Delivery>,
    #[serde(default)]
    pub max_video: Option<VideoLimits>,
    #[serde(default)]
    pub max_audio_channels: Option<u32>,
    /// Bits per second, from the client's settings. Left out: no limit.
    #[serde(default)]
    pub max_bitrate: Option<u64>,
}

impl ClientCapabilities {
    /// [`ClientCapabilities::delivery`], with the Phase 1 default filled in.
    pub fn deliveries(&self) -> Vec<Delivery> {
        if self.delivery.is_empty() {
            vec![Delivery::Range]
        } else {
            self.delivery.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_are_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&AudioCodec::TrueHd).unwrap(),
            r#""true_hd""#
        );
        assert_eq!(AudioCodec::TrueHd.as_str(), "true_hd");
    }

    #[test]
    fn a_codec_from_a_newer_peer_is_unknown() {
        assert_eq!(
            serde_json::from_str::<VideoCodec>(r#""h266""#).unwrap(),
            VideoCodec::Unknown
        );
    }

    #[test]
    fn a_phase_1_client_takes_range_reads_with_no_limits() {
        let caps: ClientCapabilities =
            serde_json::from_value(serde_json::json!({"video": ["h264"]})).unwrap();
        assert_eq!(caps.deliveries(), [Delivery::Range]);
        assert_eq!(caps.max_video, None);
        assert_eq!(caps.max_bitrate, None);
    }

    #[test]
    fn a_delivery_from_a_newer_peer_is_unknown() {
        assert_eq!(
            serde_json::from_str::<Delivery>(r#""dash""#).unwrap(),
            Delivery::Unknown
        );
    }

    #[test]
    fn capabilities_round_trip_and_default_to_empty() {
        let caps = ClientCapabilities {
            video: [VideoCodec::H264, VideoCodec::Hevc].into(),
            audio: [AudioCodec::Aac].into(),
            containers: [Container::Matroska].into(),
            subtitles: [SubtitleFormat::Srt].into(),
            ..ClientCapabilities::default()
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert_eq!(
            serde_json::from_str::<ClientCapabilities>(&json).unwrap(),
            caps
        );
        assert_eq!(
            serde_json::from_str::<ClientCapabilities>("{}").unwrap(),
            ClientCapabilities::default()
        );
    }
}
