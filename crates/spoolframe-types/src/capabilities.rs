//! Building [`ClientCapabilities`] the same way on every platform: from
//! the decoders a device found ([`ClientCapabilities::from_decoders`]),
//! or by asking its player codec string by codec string
//! ([`ClientCapabilities::probe`]: Apple's VideoToolbox, Android's
//! MediaCodecList, GStreamer), with the defaults every client set by
//! hand: PCM always, range reads then progressive when nothing else is
//! said, no height or bitrate limit, the screen's HDR as the limit.

use std::collections::BTreeSet;

use crate::codec::{
    AudioCodec, ClientCapabilities, Container, Delivery, DisplayCapabilities, HdrFormat,
    SubtitleFormat, VideoCodec, VideoLimits,
};

/// What a device's player decodes, as it found it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decoders {
    pub video: BTreeSet<VideoCodec>,
    /// Those of `video` decoded in hardware; `None` when the player does
    /// not say.
    pub hardware_video: Option<BTreeSet<VideoCodec>>,
    pub audio: BTreeSet<AudioCodec>,
    pub containers: BTreeSet<Container>,
    pub subtitles: BTreeSet<SubtitleFormat>,
    /// Preferred first; empty is range reads, then a progressive stream.
    pub delivery: Vec<Delivery>,
    pub max_audio_channels: Option<u32>,
    /// The RFC 6381 strings the player takes; `None` when not asked.
    pub video_codecs: Option<BTreeSet<String>>,
}

impl DisplayCapabilities {
    /// The screen as it is now: with `show` on (the device shows HDR, and
    /// the user's "Show HDR" is on), HDR10 and HLG; Dolby Vision too only
    /// with `dolby_vision`, where the player shows it in its own colours.
    /// Off, none: the server tone maps to SDR. A Dolby Vision profile 5
    /// film sent to a player that cannot show it has wrong colours, so it
    /// is never on by default. The bit depth is the device's.
    pub fn for_hdr(show: bool, dolby_vision: bool) -> Self {
        let mut hdr = BTreeSet::new();
        if show {
            hdr.extend([HdrFormat::Hdr10, HdrFormat::Hlg]);
            if dolby_vision {
                hdr.insert(HdrFormat::DolbyVision);
            }
        }
        Self {
            bit_depth: None,
            hdr: Some(hdr),
            video_codecs: None,
            height: None,
            frame_rate: None,
        }
    }
}

impl ClientCapabilities {
    /// What `decoders` play on `display`: PCM is always played (every
    /// player outputs it), hardware decoders are only those it plays, no
    /// delivery said is range reads then a progressive stream, and the
    /// screen's HDR and bit depth are the video limits. No height, frame
    /// rate or bitrate limit: these are the decoders' limits, and the
    /// screen's mode goes with each plan ([`ClientCapabilities::with_display`]);
    /// the bitrate is the app's setting.
    pub fn from_decoders(decoders: Decoders, display: &DisplayCapabilities) -> Self {
        let Decoders {
            video,
            hardware_video,
            mut audio,
            containers,
            subtitles,
            delivery,
            max_audio_channels,
            video_codecs,
        } = decoders;
        audio.insert(AudioCodec::Pcm);
        let hardware_video =
            hardware_video.map(|hw| hw.intersection(&video).copied().collect::<BTreeSet<_>>());
        let delivery = match delivery.is_empty() {
            true => vec![Delivery::Range, Delivery::Progressive],
            false => delivery,
        };
        Self {
            video,
            audio,
            containers,
            subtitles,
            delivery,
            max_video: Some(VideoLimits {
                height: None,
                frame_rate: None,
                bit_depth: display.bit_depth,
                hdr: display.hdr.clone(),
            }),
            max_audio_channels,
            max_bitrate: None,
            hardware_video,
            video_codecs: display.video_codecs.clone().or(video_codecs),
        }
    }

    /// The iOS app's probe, for any platform: `player`'s fixed lists (its
    /// audio, containers, subtitles, delivery, and the video it plays in
    /// software), plus each codec of its `hardware_video` (what decodes
    /// in hardware, as the platform says), on `display`. `probe_codec` is
    /// asked about each of [`candidate_codec_strings`]. The bit depth is
    /// 10 where HEVC decodes in hardware (Main 10), else 8, unless
    /// `display` says its own.
    pub fn probe(
        probe_codec: &dyn CodecProbe,
        display: &DisplayCapabilities,
        player: Decoders,
    ) -> Self {
        let hardware = player.hardware_video.clone().unwrap_or_default();
        let mut video = player.video;
        video.extend(hardware.iter().copied());
        let video_codecs = candidate_codec_strings()
            .into_iter()
            .filter(|s| probe_codec.plays(s))
            .collect();
        let bit_depth = match hardware.contains(&VideoCodec::Hevc) {
            true => 10,
            false => 8,
        };
        let display = DisplayCapabilities {
            bit_depth: display.bit_depth.or(Some(bit_depth)),
            ..display.clone()
        };
        Self::from_decoders(
            Decoders {
                video,
                hardware_video: Some(hardware),
                video_codecs: Some(video_codecs),
                ..player
            },
            &display,
        )
    }
}

/// A device's player, asked about an RFC 6381 codec string
/// ([`ClientCapabilities::probe`]): VideoToolbox and AVURLAsset,
/// MediaCodecList, GStreamer. Any `Fn(&str) -> bool` is one.
pub trait CodecProbe {
    /// Whether it plays MP4 video of codec string `codec`.
    fn plays(&self, codec: &str) -> bool;
}

impl<F: Fn(&str) -> bool> CodecProbe for F {
    fn plays(&self, codec: &str) -> bool {
        self(codec)
    }
}

/// The codec strings a probe asks about (the iOS app's list): H.264 High
/// at levels 4.1 to 5.2; HEVC Main and Main 10, both tiers, levels 4.1 to
/// 6.2; Dolby Vision profiles 5, 7 and 8, levels 1 to 13; AV1 Main, both
/// tiers, 8- and 10-bit, levels 4.0 to 6.1.
pub fn candidate_codec_strings() -> Vec<String> {
    let mut out = Vec::new();
    for level in ["29", "2A", "32", "33", "34"] {
        out.push(format!("avc1.6400{level}"));
    }
    for tag in ["hvc1", "hev1"] {
        for profile in ["1.6", "2.4"] {
            for tier in ["L", "H"] {
                for level in [123, 150, 153, 156, 180, 183, 186] {
                    out.push(format!("{tag}.{profile}.{tier}{level}.B0"));
                }
            }
        }
    }
    for tag in ["dvh1", "dvhe"] {
        for profile in ["05", "07", "08"] {
            for level in 1..=13 {
                out.push(format!("{tag}.{profile}.{level:02}"));
            }
        }
    }
    for tier in ["M", "H"] {
        for level in ["08", "09", "12", "13", "14", "16", "17"] {
            for depth in ["08", "10"] {
                out.push(format!("av01.0.{level}{tier}.{depth}"));
            }
        }
    }
    out
}

impl VideoCodec {
    /// The codec of an Android `MediaCodec` MIME type (`video/hevc`), as
    /// `MediaCodecList` names its decoders; `None` for one Spoolframe has
    /// no name for.
    pub fn for_mime(mime: &str) -> Option<Self> {
        Some(match mime {
            "video/avc" => Self::H264,
            "video/hevc" => Self::Hevc,
            "video/av01" => Self::Av1,
            "video/x-vnd.on2.vp9" => Self::Vp9,
            "video/x-vnd.on2.vp8" => Self::Vp8,
            "video/mpeg2" => Self::Mpeg2,
            "video/mp4v-es" => Self::Mpeg4,
            "video/wvc1" => Self::Vc1,
            _ => return None,
        })
    }
}

impl AudioCodec {
    /// The codec of an Android `MediaCodec` MIME type (`audio/eac3`), as
    /// `MediaCodecList` names its decoders; `None` for one Spoolframe has
    /// no name for. E-AC-3 with Atmos (JOC) is E-AC-3, and DTS-HD is DTS:
    /// the core is what plays.
    pub fn for_mime(mime: &str) -> Option<Self> {
        Some(match mime {
            "audio/mp4a-latm" => Self::Aac,
            "audio/ac3" => Self::Ac3,
            "audio/eac3" | "audio/eac3-joc" => Self::Eac3,
            "audio/vnd.dts" | "audio/vnd.dts.hd" => Self::Dts,
            "audio/true-hd" => Self::TrueHd,
            "audio/flac" => Self::Flac,
            "audio/alac" => Self::Alac,
            "audio/opus" => Self::Opus,
            "audio/vorbis" => Self::Vorbis,
            "audio/mpeg" => Self::Mp3,
            "audio/raw" => Self::Pcm,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_s_decoder_types_name_their_codecs() {
        assert_eq!(
            (
                VideoCodec::for_mime("video/hevc"),
                VideoCodec::for_mime("video/dolby-vision"),
                AudioCodec::for_mime("audio/eac3-joc"),
                AudioCodec::for_mime("audio/vnd.dts.hd"),
                AudioCodec::for_mime("audio/3gpp"),
            ),
            (
                Some(VideoCodec::Hevc),
                None,
                Some(AudioCodec::Eac3),
                Some(AudioCodec::Dts),
                None
            )
        );
    }

    /// The probe says the decoders' limits only: the screen's mode is
    /// not Hello's, so a TV that signs in at 1080p is not held there.
    #[test]
    fn a_probe_leaves_the_screen_s_mode_to_each_plan() {
        let display = DisplayCapabilities {
            height: Some(1080),
            frame_rate: Some(60),
            ..DisplayCapabilities::for_hdr(false, false)
        };
        let limits = ClientCapabilities::probe(&|_: &str| true, &display, apple_tv())
            .max_video
            .unwrap();
        assert_eq!((limits.height, limits.frame_rate), (None, None));
    }

    /// Signed in on a 1080p screen, switched to 2160p since: a decoder
    /// that plays 2160 gets 2160 at the next plan.
    #[test]
    fn a_screen_switched_up_since_the_sign_in_plays_its_new_mode() {
        let hello = ClientCapabilities::probe(
            &|_: &str| true,
            &DisplayCapabilities {
                height: Some(1080),
                ..DisplayCapabilities::for_hdr(false, false)
            },
            apple_tv(),
        );
        let at_2160 = DisplayCapabilities {
            height: Some(2160),
            ..DisplayCapabilities::for_hdr(false, false)
        };
        let limits = hello.with_display(Some(&at_2160)).max_video.unwrap();
        assert_eq!(limits.height, Some(2160));
    }

    #[test]
    fn a_display_without_a_mode_keeps_hello_s_height_and_frame_rate() {
        let mut hello = ClientCapabilities::from_decoders(
            apple_tv(),
            &DisplayCapabilities::for_hdr(false, false),
        );
        hello.max_video.as_mut().unwrap().height = Some(2160);
        let at_1080 = DisplayCapabilities {
            height: Some(1080),
            ..DisplayCapabilities::for_hdr(true, false)
        };
        let heights = [&DisplayCapabilities::for_hdr(true, false), &at_1080]
            .map(|display| hello.with_display(Some(display)).max_video.unwrap().height);
        assert_eq!(heights, [Some(2160), Some(1080)]);
    }

    /// A 4K screen does not raise a 1080p decoder's limit: the lower of
    /// the two holds.
    #[test]
    fn a_display_s_mode_never_raises_hello_s_limits() {
        let mut hello = ClientCapabilities::from_decoders(
            apple_tv(),
            &DisplayCapabilities::for_hdr(false, false),
        );
        let limits = hello.max_video.as_mut().unwrap();
        limits.height = Some(1080);
        limits.frame_rate = Some(30);
        let at_2160_60 = DisplayCapabilities {
            height: Some(2160),
            frame_rate: Some(60),
            ..DisplayCapabilities::for_hdr(true, false)
        };
        let limits = hello.with_display(Some(&at_2160_60)).max_video.unwrap();
        assert_eq!((limits.height, limits.frame_rate), (Some(1080), Some(30)));
    }

    #[test]
    fn hdr_on_is_hdr10_and_hlg_and_dolby_vision_only_when_asked() {
        assert_eq!(
            DisplayCapabilities::for_hdr(false, true).hdr,
            Some(BTreeSet::new())
        );
        assert_eq!(
            DisplayCapabilities::for_hdr(true, false).hdr,
            Some([HdrFormat::Hdr10, HdrFormat::Hlg].into())
        );
        assert_eq!(
            DisplayCapabilities::for_hdr(true, true).hdr,
            Some([HdrFormat::Hdr10, HdrFormat::Hlg, HdrFormat::DolbyVision].into())
        );
    }

    #[test]
    fn decoders_always_play_pcm_and_default_to_range_then_progressive() {
        let caps = ClientCapabilities::from_decoders(
            Decoders {
                video: [VideoCodec::H264].into(),
                hardware_video: Some([VideoCodec::H264, VideoCodec::Av1].into()),
                audio: [AudioCodec::Aac].into(),
                ..Decoders::default()
            },
            &DisplayCapabilities::for_hdr(false, false),
        );
        assert!(caps.audio.contains(&AudioCodec::Pcm));
        assert_eq!(caps.deliveries(), [Delivery::Range, Delivery::Progressive]);
        assert_eq!(
            caps.hardware_video,
            Some([VideoCodec::H264].into()),
            "only what it plays"
        );
        let limits = caps.max_video.unwrap();
        assert_eq!(
            (limits.height, limits.bit_depth, limits.hdr),
            (None, None, Some(BTreeSet::new()))
        );
        assert_eq!(caps.max_bitrate, None);
    }

    /// `player`, decoding `codecs` in hardware.
    fn hardware(player: Decoders, codecs: &[VideoCodec]) -> Decoders {
        Decoders {
            hardware_video: Some(codecs.iter().copied().collect()),
            ..player
        }
    }

    /// An Apple TV's lists: AAC, E-AC-3 and the rest, MP4, WebVTT, HLS.
    fn apple_tv() -> Decoders {
        Decoders {
            video: [VideoCodec::H264, VideoCodec::Hevc].into(),
            audio: [AudioCodec::Aac, AudioCodec::Eac3].into(),
            containers: [Container::Mp4].into(),
            subtitles: [SubtitleFormat::WebVtt].into(),
            delivery: vec![Delivery::Hls, Delivery::Range],
            max_audio_channels: Some(8),
            ..Decoders::default()
        }
    }

    #[test]
    fn a_probe_asks_the_player_for_each_codec_string() {
        let caps = ClientCapabilities::probe(
            &|codec: &str| codec.starts_with("hvc1.2.4.L"),
            &DisplayCapabilities::for_hdr(true, false),
            hardware(
                apple_tv(),
                &[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1],
            ),
        );
        assert_eq!(
            caps.video,
            [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1].into(),
            "AV1 is played where it decodes in hardware"
        );
        assert!(caps.decodes_in_hardware(VideoCodec::Av1));
        assert_eq!(
            caps.audio,
            [AudioCodec::Aac, AudioCodec::Eac3, AudioCodec::Pcm].into()
        );
        assert_eq!(caps.deliveries(), [Delivery::Hls, Delivery::Range]);
        let strings = caps.video_codecs.unwrap();
        assert_eq!(strings.len(), 7);
        assert!(strings.contains("hvc1.2.4.L153.B0"));
        assert_eq!(caps.max_audio_channels, Some(8));
    }

    #[test]
    fn a_probe_is_10_bit_only_where_hevc_decodes_in_hardware() {
        let depth = |decoded: &[VideoCodec]| {
            ClientCapabilities::probe(
                &|_: &str| true,
                &DisplayCapabilities::for_hdr(true, false),
                hardware(apple_tv(), decoded),
            )
            .max_video
            .unwrap()
            .bit_depth
        };
        assert_eq!(depth(&[VideoCodec::H264, VideoCodec::Hevc]), Some(10));
        assert_eq!(depth(&[VideoCodec::H264]), Some(8));
        assert!(
            !ClientCapabilities::probe(
                &|_: &str| true,
                &DisplayCapabilities::for_hdr(true, false),
                hardware(apple_tv(), &[VideoCodec::H264]),
            )
            .decodes_in_hardware(VideoCodec::Hevc)
        );
    }

    #[test]
    fn the_candidates_are_the_ios_list() {
        let all = candidate_codec_strings();
        assert_eq!(all.len(), 5 + 56 + 78 + 28);
        assert!(all.contains(&"dvh1.08.06".to_owned()));
        assert!(all.contains(&"av01.0.12M.10".to_owned()));
    }
}
