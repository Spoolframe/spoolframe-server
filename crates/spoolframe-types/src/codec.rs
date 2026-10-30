//! The words server and client share for formats.
//!
//! The server knows streams by ffprobe's names (`hevc`, `hdmv_pgs_subtitle`)
//! and a GStreamer client knows its decoders by caps (`video/x-h265`);
//! compared as strings they never match. Each side maps its own names to
//! these enums instead. A format either side does not know is `Unknown`,
//! which no client supports, so a plan says so rather than guessing.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::CodecString;

macro_rules! vocabulary {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident => $wire:literal,)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

impl Container {
    /// The MIME type a file in it is served as, by HTTP or to a player
    /// that reads bytes. One a player has no type for is
    /// `application/octet-stream`: the player sniffs it.
    pub fn mime(self) -> &'static str {
        match self {
            Container::Mp4 => "video/mp4",
            Container::Matroska => "video/x-matroska",
            Container::MpegTs => "video/mp2t",
            Container::Avi => "video/x-msvideo",
            Container::Flac => "audio/flac",
            Container::Mp3 => "audio/mpeg",
            Container::Ogg => "audio/ogg",
            _ => "application/octet-stream",
        }
    }
}

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
    /// How a client takes media in.
    Delivery {
        /// Range reads of the file itself.
        Range => "range",
        /// HLS playlists and segments.
        Hls => "hls",
        /// One stream written as it is encoded, read by range.
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

/// The most video a client can show. Each limit that is `None` is no
/// limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoLimits {
    /// Lines, e.g. 1080. At Hello, what the decoders take: the screen's
    /// mode goes with each plan ([`DisplayCapabilities::height`]).
    #[serde(deserialize_with = "crate::required::option")]
    pub height: Option<u32>,
    /// Frames per second. At Hello, what the decoders take, as `height`.
    #[serde(deserialize_with = "crate::required::option")]
    pub frame_rate: Option<u32>,
    /// Bits per sample, e.g. 8 for a client without 10-bit decoding.
    #[serde(deserialize_with = "crate::required::option")]
    pub bit_depth: Option<u32>,
    /// The HDR formats it shows as HDR; anything else is tone-mapped to
    /// SDR. `None`: all of them.
    #[serde(deserialize_with = "crate::required::option")]
    pub hdr: Option<BTreeSet<HdrFormat>>,
}

/// What a client can play. Belongs to the endpoint, not to a session: two
/// profiles on one device have the same decoders. A client builds it from
/// what it actually has (the Linux client from the GStreamer registry),
/// not from a fixed list.
///
/// Every field is required on the wire: an empty set or `null`
/// says "none", and a client that leaves a field out is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    pub video: BTreeSet<VideoCodec>,
    pub audio: BTreeSet<AudioCodec>,
    pub containers: BTreeSet<Container>,
    pub subtitles: BTreeSet<SubtitleFormat>,
    /// How it takes media in, preferred first. Empty means range reads
    /// only.
    pub delivery: Vec<Delivery>,
    #[serde(deserialize_with = "crate::required::option")]
    pub max_video: Option<VideoLimits>,
    #[serde(deserialize_with = "crate::required::option")]
    pub max_audio_channels: Option<u32>,
    /// Bits per second, from the client's settings. `None`: no limit.
    #[serde(deserialize_with = "crate::required::option")]
    pub max_bitrate: Option<u64>,
    /// The video codecs of [`ClientCapabilities::video`] it decodes in
    /// hardware (PGPU-06): a conversion goes to a codec the GPU encodes
    /// only if the device decodes it in hardware, so AV1 is not sent to a
    /// device that would decode it in software, draining its battery.
    /// `None`: it does not say, and every codec it plays counts. Unlike
    /// the fields above it may be left out, and is `None` then.
    #[serde(default)]
    pub hardware_video: Option<BTreeSet<VideoCodec>>,
    /// The RFC 6381 codec strings it plays (`hvc1.2.4.L153.B0`,
    /// `dvh1.08.06`; [`CodecString`]), as its player answers for each:
    /// a video is copied only where one of them covers the source's own
    /// string (its profile, tier and level, its Dolby Vision profile).
    /// One the server does not read is passed over. `None`: it does not
    /// say, and [`ClientCapabilities::video`] and `max_video` alone
    /// decide. It may be left out, and is `None` then.
    #[serde(default)]
    pub video_codecs: Option<BTreeSet<String>>,
}

/// What the screen shows now, as a playback starts: [`VideoLimits::hdr`]
/// and [`VideoLimits::bit_depth`] for that playback only, over what the
/// client said at Hello. A TV's display mode changes while the app runs
/// (Match Dynamic Range, another TV or receiver, HDR turned off), and
/// capabilities from Hello would send HDR to an SDR mode or tone map for
/// an HDR one.
///
/// Both fields are required, as in [`VideoLimits`]: `None` is no limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayCapabilities {
    /// As [`VideoLimits::bit_depth`].
    #[serde(deserialize_with = "crate::required::option")]
    pub bit_depth: Option<u32>,
    /// As [`VideoLimits::hdr`].
    #[serde(deserialize_with = "crate::required::option")]
    pub hdr: Option<BTreeSet<HdrFormat>>,
    /// As [`ClientCapabilities::video_codecs`], asked again for this
    /// display: a Dolby Vision string may be played on one mode and not
    /// on another. `None` keeps Hello's. It may be left out, and is
    /// `None` then.
    #[serde(default)]
    pub video_codecs: Option<BTreeSet<String>>,
    /// As [`VideoLimits::height`], the screen's mode now: a TV set to
    /// 1080p is not sent 4K. It lowers Hello's (the decoders') limit,
    /// never raises it; `None` keeps Hello's. It may be left out.
    #[serde(default)]
    pub height: Option<u32>,
    /// As [`VideoLimits::frame_rate`], the mode's refresh rate. `None`
    /// keeps Hello's. It may be left out.
    #[serde(default)]
    pub frame_rate: Option<u32>,
}

/// A height or frame rate as a limit: 0 lines or 0 frames a second is a
/// value not said, not a cap, which would convert everything to nothing.
fn said(limit: Option<u32>) -> Option<u32> {
    limit.filter(|v| *v > 0)
}

/// The lower of two limits, `None` being none: either one where only one
/// is set.
fn lower(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (said(a), said(b)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl ClientCapabilities {
    /// The capabilities a plan reads: these, with `display`'s HDR formats
    /// and bit depth in place of [`ClientCapabilities::max_video`]'s, and
    /// its height and frame rate as limits where it says them, never above
    /// Hello's. With or without a `display`, a height or frame rate of 0
    /// is read as not said: the limits it gives never hold a 0.
    pub fn with_display(&self, display: Option<&DisplayCapabilities>) -> ClientCapabilities {
        let mut caps = self.clone();
        if let Some(display) = display {
            let limits = caps.max_video.get_or_insert_with(VideoLimits::default);
            limits.hdr = display.hdr.clone();
            limits.bit_depth = display.bit_depth;
            // The screen's mode lowers Hello's limits, never raises them:
            // a 4K TV does not lift a 1080p decoder's cap.
            limits.height = lower(limits.height, display.height);
            limits.frame_rate = lower(limits.frame_rate, display.frame_rate);
            if display.video_codecs.is_some() {
                caps.video_codecs = display.video_codecs.clone();
            }
        }
        if let Some(limits) = caps.max_video.as_mut() {
            limits.height = said(limits.height);
            limits.frame_rate = said(limits.frame_rate);
        }
        caps
    }

    /// Whether it plays a video whose own codec string is `source`, by
    /// [`ClientCapabilities::video_codecs`]: some string listed there
    /// [covers](CodecString::covers) it. `None` when it lists none (it
    /// does not say), and nothing is known of it here.
    pub fn plays_codec_string(&self, source: &CodecString) -> Option<bool> {
        let listed = self.video_codecs.as_ref()?;
        Some(
            listed
                .iter()
                .filter_map(|s| s.parse::<CodecString>().ok())
                .any(|listed| listed.covers(source)),
        )
    }

    /// Whether it decodes `codec` in hardware, as it says; every codec it
    /// plays when it does not say (PGPU-06).
    pub fn decodes_in_hardware(&self, codec: VideoCodec) -> bool {
        self.video.contains(&codec)
            && self
                .hardware_video
                .as_ref()
                .is_none_or(|hardware| hardware.contains(&codec))
    }

    /// [`ClientCapabilities::delivery`], with the default (range reads
    /// only) filled in when it is empty.
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

    /// A start's display replaces Hello's HDR formats and bit depth, and
    /// nothing else (PGPU-06); none leaves them as they are.
    #[test]
    fn a_display_replaces_hdr_and_bit_depth_only() {
        let hello = ClientCapabilities {
            video: [VideoCodec::Hevc].into(),
            max_video: Some(VideoLimits {
                height: Some(1080),
                frame_rate: Some(60),
                bit_depth: Some(10),
                hdr: Some([HdrFormat::Hdr10, HdrFormat::DolbyVision].into()),
            }),
            ..ClientCapabilities::default()
        };
        assert_eq!(hello.with_display(None), hello);
        let sdr = DisplayCapabilities {
            bit_depth: Some(8),
            hdr: Some(BTreeSet::new()),
            video_codecs: None,
            height: None,
            frame_rate: None,
        };
        let now = hello.with_display(Some(&sdr));
        assert_eq!(
            now.max_video,
            Some(VideoLimits {
                height: Some(1080),
                frame_rate: Some(60),
                bit_depth: Some(8),
                hdr: Some(BTreeSet::new()),
            })
        );
        assert_eq!(now.video, hello.video);
        // A client with no limits at Hello gets the display's.
        let unlimited = ClientCapabilities::default().with_display(Some(&sdr));
        assert_eq!(unlimited.max_video.unwrap().hdr, Some(BTreeSet::new()));
    }

    /// Hello's height or frame rate of 0, with no display, is not said
    /// either: the limits a plan reads never hold a 0.
    #[test]
    fn hello_at_zero_with_no_display_is_no_limit() {
        let hello = ClientCapabilities {
            max_video: Some(VideoLimits {
                height: Some(0),
                frame_rate: Some(0),
                bit_depth: Some(10),
                hdr: None,
            }),
            ..ClientCapabilities::default()
        };
        assert_eq!(
            hello.with_display(None).max_video,
            Some(VideoLimits {
                height: None,
                frame_rate: None,
                bit_depth: Some(10),
                hdr: None,
            })
        );
    }

    /// A height or frame rate of 0 is a screen that did not say, on
    /// either side: it caps nothing, rather than every plan at 0 lines.
    #[test]
    fn a_display_at_zero_changes_no_limit() {
        let limits = |height, frame_rate| ClientCapabilities {
            max_video: Some(VideoLimits {
                height,
                frame_rate,
                ..VideoLimits::default()
            }),
            ..ClientCapabilities::default()
        };
        let mode = |height, frame_rate| DisplayCapabilities {
            height,
            frame_rate,
            ..DisplayCapabilities::default()
        };
        let limited = |caps: ClientCapabilities, display| {
            let video = caps.with_display(Some(&display)).max_video.unwrap();
            (video.height, video.frame_rate)
        };
        assert_eq!(
            [
                limited(limits(Some(2160), Some(60)), mode(Some(0), Some(0))),
                limited(limits(None, None), mode(Some(0), Some(0))),
                limited(limits(Some(0), Some(0)), mode(Some(1080), Some(30))),
            ],
            [(Some(2160), Some(60)), (None, None), (Some(1080), Some(30))]
        );
    }

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

    /// PGPU-06: a client that leaves `hardware_video` out, as every one
    /// of revision 14 does, is read, every codec it plays counting as
    /// hardware; one that says counts only those.
    #[test]
    fn hardware_decoders_may_be_left_out() {
        let old = r#"{"video":["hevc","av1"],"audio":[],"containers":[],"subtitles":[],
            "delivery":[],"max_video":null,"max_audio_channels":null,"max_bitrate":null}"#;
        let caps: ClientCapabilities = serde_json::from_str(old).unwrap();
        assert_eq!(caps.hardware_video, None);
        assert!(caps.decodes_in_hardware(VideoCodec::Av1));
        assert!(
            !caps.decodes_in_hardware(VideoCodec::H264),
            "not played at all"
        );
        let said = ClientCapabilities {
            hardware_video: Some([VideoCodec::Hevc].into()),
            ..caps
        };
        assert!(said.decodes_in_hardware(VideoCodec::Hevc));
        assert!(!said.decodes_in_hardware(VideoCodec::Av1));
        let wire = serde_json::to_value(&said).unwrap();
        assert_eq!(wire["hardware_video"], serde_json::json!(["hevc"]));
    }

    /// The Apple TV of the ticket (plex:movie:1936): its list read as
    /// `video_codecs`, a string it does not list refused, one the server
    /// cannot read passed over; a client that left the field out says
    /// nothing.
    #[test]
    fn a_client_lists_the_codec_strings_it_plays() {
        let hello = r#"{"video":["hevc"],"audio":[],"containers":[],"subtitles":[],
            "delivery":[],"max_video":null,"max_audio_channels":null,"max_bitrate":null,
            "hardware_video":["hevc"],
            "video_codecs":["hvc1.2.4.L153.B0","dvh1.08.06","vp09.00.10.08","nonsense"]}"#;
        let caps: ClientCapabilities = serde_json::from_str(hello).unwrap();
        let judge = |s: &str| caps.plays_codec_string(&s.parse().unwrap());
        assert_eq!(judge("hvc1.2.4.L150.B0"), Some(true));
        assert_eq!(judge("hvc1.2.4.L186.B0"), Some(false));
        assert_eq!(judge("dvh1.08.06"), Some(true));
        assert_eq!(judge("dvh1.07.06"), Some(false));
        let wire = serde_json::to_value(&caps).unwrap();
        assert_eq!(wire["video_codecs"][0], "dvh1.08.06", "a sorted set");
        let unsaid = ClientCapabilities {
            video_codecs: None,
            ..caps.clone()
        };
        assert_eq!(
            unsaid.plays_codec_string(&"hvc1.2.4.L186.B0".parse().unwrap()),
            None
        );
        let old = r#"{"video":["hevc"],"audio":[],"containers":[],"subtitles":[],
            "delivery":[],"max_video":null,"max_audio_channels":null,"max_bitrate":null}"#;
        let old: ClientCapabilities = serde_json::from_str(old).unwrap();
        assert_eq!(old.video_codecs, None);
    }

    /// A start's display may list the strings again (its Dolby Vision
    /// answer is that mode's); one that does not keeps Hello's.
    #[test]
    fn a_display_s_codec_strings_replace_hello_s() {
        let hello = ClientCapabilities {
            video: [VideoCodec::Hevc].into(),
            video_codecs: Some(["dvh1.08.06".to_owned(), "hvc1.2.4.L153.B0".to_owned()].into()),
            ..ClientCapabilities::default()
        };
        let sdr = DisplayCapabilities {
            bit_depth: Some(8),
            hdr: Some(BTreeSet::new()),
            video_codecs: None,
            height: None,
            frame_rate: None,
        };
        assert_eq!(
            hello.with_display(Some(&sdr)).video_codecs,
            hello.video_codecs
        );
        let hdr10 = DisplayCapabilities {
            video_codecs: Some(["hvc1.2.4.L153.B0".to_owned()].into()),
            ..sdr
        };
        assert_eq!(
            hello.with_display(Some(&hdr10)).video_codecs,
            hdr10.video_codecs
        );
        let json = r#"{"bit_depth":10,"hdr":null}"#;
        let display: DisplayCapabilities = serde_json::from_str(json).unwrap();
        assert_eq!(display.video_codecs, None);
    }

    #[test]
    fn a_client_without_deliveries_takes_range_reads() {
        let caps = ClientCapabilities {
            video: [VideoCodec::H264].into(),
            ..ClientCapabilities::default()
        };
        assert_eq!(caps.deliveries(), [Delivery::Range]);
    }

    /// Capabilities were optional field by field, so a client that left
    /// one out was planned for as if it had none (PX-15).
    #[test]
    fn capabilities_missing_a_field_are_refused() {
        let caps = ClientCapabilities {
            max_video: Some(VideoLimits::default()),
            ..ClientCapabilities::default()
        };
        crate::test_support::assert_each_required(
            &caps,
            &[
                "video",
                "audio",
                "containers",
                "subtitles",
                "delivery",
                "max_video",
                "max_audio_channels",
                "max_bitrate",
            ],
        );
        crate::test_support::assert_each_required(
            &VideoLimits::default(),
            &["height", "frame_rate", "bit_depth", "hdr"],
        );
    }

    #[test]
    fn a_delivery_from_a_newer_peer_is_unknown() {
        assert_eq!(
            serde_json::from_str::<Delivery>(r#""dash""#).unwrap(),
            Delivery::Unknown
        );
    }

    #[test]
    fn capabilities_round_trip() {
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
    }
}
