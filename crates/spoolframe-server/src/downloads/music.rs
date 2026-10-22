//! How a track's download is written (PX-51): the file as it is, a remux
//! into a container the device plays, or its audio converted by the pool.
//!
//! In original quality (the user's decisions, 2026-10-14): the file as
//! it is when the device class plays it, byte for byte. A codec the
//! class plays in a container it does not is remuxed, nothing encoded.
//! Else the closest the class plays: lossless stays lossless (ALAC on
//! Apple, FLAC on Linux), and a lossy file becomes AAC at
//! [`BEST_AAC`]. At 256 kbit/s, AAC at [`AAC_256`], unless the file is
//! lossy at about that rate or less: encoding it again would only lose
//! more and grow it, so it is sent as it is (or remuxed).
//!
//! What each class plays as it is: Apple's players (AVPlayer) take AAC,
//! ALAC, AC-3 and E-AC-3 in MP4, MP3 in MP3 or MP4, and FLAC in FLAC or
//! MP4; not Ogg, nor Opus or Vorbis. The Linux app (GStreamer) plays
//! every codec [`DeviceClass::keep_audio`] names in any container the
//! server names but MPEG-TS and AVI. Anything the server does not name
//! (APE, WavPack, WMA, DSD…) is converted for both.
//!
//! A converted or remuxed file keeps the file's tags (`-map_metadata 0`)
//! and its embedded cover, where the container takes one.

use serde::{Deserialize, Serialize};

use spoolframe_media::{MediaDescriptor, codecs};
use spoolframe_types::{AudioCodec, Container};

use super::DeviceClass;
use super::plan::{Decision, PlanError};
use crate::store::optimized::{AudioRecipe, CopyTarget, CopyVideo};

/// AAC's rate for a lossy file converted in original quality: the best
/// a player tells from the source, in stereo.
pub const BEST_AAC: u64 = 320_000;

/// AAC's rate in the 256 kbit/s quality.
pub const AAC_256: u64 = 256_000;

/// How far over [`AAC_256`] a lossy file may be and still be sent as it
/// is in that quality: its file's rate counts its tags and cover too.
const AAC_256_SLACK: u64 = AAC_256 / 10;

/// A track download's quality (a setting per device).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicQuality {
    /// The file, converted only where the device needs it.
    Original,
    /// AAC at 256 kbit/s.
    Aac256,
}

impl MusicQuality {
    /// The name in a copy's target, in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Aac256 => "aac256",
        }
    }

    /// The quality `s` names, as [`MusicQuality::as_str`] spells it.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "original" => Some(Self::Original),
            "aac256" => Some(Self::Aac256),
            _ => None,
        }
    }
}

/// The container a track's file is written in, when it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MusicFormat {
    /// `.m4a`: AAC, ALAC, AC-3, E-AC-3.
    Mp4,
    Flac,
    Mp3,
    /// Opus and Vorbis, for Linux. Its cover is left out: ffmpeg's Ogg
    /// muxer takes no picture stream.
    Ogg,
    /// `.mka`: what Linux plays in no container of its own (DTS, PCM).
    Matroska,
}

impl MusicFormat {
    /// Every format, for the names the download area writes.
    pub const ALL: [Self; 5] = [Self::Mp4, Self::Flac, Self::Mp3, Self::Ogg, Self::Matroska];

    pub fn container(self) -> Container {
        match self {
            Self::Mp4 => Container::Mp4,
            Self::Flac => Container::Flac,
            Self::Mp3 => Container::Mp3,
            Self::Ogg => Container::Ogg,
            Self::Matroska => Container::Matroska,
        }
    }

    /// The file's extension in the area.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "m4a",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
            Self::Matroska => "mka",
        }
    }

    /// ffmpeg's muxer for it.
    pub fn muxer(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
            Self::Matroska => "matroska",
        }
    }

    /// Whether the file's cover goes in it.
    pub fn takes_cover(self) -> bool {
        self != Self::Ogg
    }
}

/// How a track's download is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicPlan {
    /// As a film's: as it is, a remux, or converted by the pool.
    pub decision: Decision,
    /// What the file is.
    pub container: Container,
    /// What it is written as; `None` when it is the file as it is.
    pub format: Option<MusicFormat>,
}

/// Whether a lossless codec: ffprobe's name of the track's.
pub fn lossless(ffprobe: &str) -> bool {
    matches!(
        ffprobe,
        "flac"
            | "alac"
            | "ape"
            | "wavpack"
            | "tta"
            | "tak"
            | "shorten"
            | "mlp"
            | "truehd"
            | "wmalossless"
            | "dst"
    ) || ffprobe.starts_with("pcm_")
        || ffprobe.starts_with("dsd_")
}

/// Whether a device of `class` plays `codec` at all, in some container.
fn plays_codec(class: DeviceClass, codec: AudioCodec) -> bool {
    match class {
        DeviceClass::Apple => matches!(
            codec,
            AudioCodec::Aac
                | AudioCodec::Alac
                | AudioCodec::Ac3
                | AudioCodec::Eac3
                | AudioCodec::Mp3
                | AudioCodec::Flac
        ),
        DeviceClass::Linux => class.keep_audio().contains(&codec),
    }
}

/// Whether a device of `class` plays `codec` in `container` as it is.
pub fn plays(class: DeviceClass, codec: AudioCodec, container: Container) -> bool {
    if !plays_codec(class, codec) {
        return false;
    }
    match class {
        DeviceClass::Apple => match codec {
            AudioCodec::Mp3 => matches!(container, Container::Mp3 | Container::Mp4),
            AudioCodec::Flac => matches!(container, Container::Flac | Container::Mp4),
            _ => container == Container::Mp4,
        },
        DeviceClass::Linux => matches!(
            container,
            Container::Mp4
                | Container::Flac
                | Container::Mp3
                | Container::Ogg
                | Container::Matroska
        ),
    }
}

/// The container a codec the class plays is remuxed into.
fn remux_format(codec: AudioCodec) -> MusicFormat {
    match codec {
        AudioCodec::Flac => MusicFormat::Flac,
        AudioCodec::Mp3 => MusicFormat::Mp3,
        AudioCodec::Opus | AudioCodec::Vorbis => MusicFormat::Ogg,
        AudioCodec::Aac | AudioCodec::Alac | AudioCodec::Ac3 | AudioCodec::Eac3 => MusicFormat::Mp4,
        _ => MusicFormat::Matroska,
    }
}

/// The track a copy of `descriptor`'s file for `class` in `quality`
/// stores, if it converts: what [`plan`] decides and the pool makes.
pub fn recipe(
    descriptor: &MediaDescriptor,
    class: DeviceClass,
    quality: MusicQuality,
) -> Option<AudioRecipe> {
    let audio = descriptor.audio.first()?;
    let channels = audio.channels.unwrap_or(2).max(1);
    let lossless = lossless(&audio.codec);
    let (codec, bitrate) = match quality {
        MusicQuality::Aac256 => (AudioCodec::Aac, AAC_256),
        MusicQuality::Original if lossless => match class {
            DeviceClass::Apple => (AudioCodec::Alac, 0),
            DeviceClass::Linux => (AudioCodec::Flac, 0),
        },
        MusicQuality::Original => (AudioCodec::Aac, BEST_AAC),
    };
    Some(AudioRecipe {
        stream: audio.index,
        codec,
        channels,
        bitrate,
    })
}

/// The video a track's copy is asked with: none, as if copied, so the
/// pool stores no video.
pub const NO_VIDEO: CopyVideo = CopyVideo {
    codec: spoolframe_types::VideoCodec::Unknown,
    height: 0,
    max_bitrate: None,
    tone_map: false,
    copied: true,
};

/// How a download of track `descriptor`'s file is written for a `class`
/// device in `quality`.
pub fn plan(
    descriptor: &MediaDescriptor,
    class: DeviceClass,
    quality: MusicQuality,
) -> Result<MusicPlan, PlanError> {
    let audio = descriptor.audio.first().ok_or(PlanError::NoAudio(0))?;
    let codec = codecs::audio_codec(&audio.codec);
    let container = codecs::container(&descriptor.container);
    let kept = match quality {
        MusicQuality::Original => true,
        // A lossy file about 256 kbit/s or under: as good as it gets.
        MusicQuality::Aac256 => {
            !lossless(&audio.codec)
                && descriptor
                    .bit_rate
                    .is_some_and(|rate| rate <= AAC_256 + AAC_256_SLACK)
        }
    };
    if kept && plays(class, codec, container) {
        return Ok(MusicPlan {
            decision: Decision::AsIs,
            container,
            format: None,
        });
    }
    if kept && plays_codec(class, codec) {
        let format = remux_format(codec);
        return Ok(MusicPlan {
            decision: Decision::Remux,
            container: format.container(),
            format: Some(format),
        });
    }
    let recipe = recipe(descriptor, class, quality).ok_or(PlanError::NoAudio(0))?;
    let format = match recipe.codec {
        AudioCodec::Flac => MusicFormat::Flac,
        _ => MusicFormat::Mp4,
    };
    Ok(MusicPlan {
        decision: Decision::Convert {
            target: CopyTarget::Music { class, quality },
            video: NO_VIDEO,
            audio: vec![audio.index],
        },
        container: format.container(),
        format: Some(format),
    })
}

/// About how many bytes a track's download is, `decision` as [`plan`]
/// made it for `class` and `quality` of `descriptor`'s file, `size`
/// bytes: the file's own size when it is kept, remuxed or converted
/// losslessly (ALAC or FLAC from another lossless codec is about as
/// large), else the AAC's rate over the track's length. `None` for AAC
/// when the length is not known.
pub fn estimate(
    descriptor: &MediaDescriptor,
    class: DeviceClass,
    quality: MusicQuality,
    decision: &Decision,
    size: u64,
) -> Option<u64> {
    if !matches!(decision, Decision::Convert { .. }) {
        return Some(size);
    }
    let rate = recipe(descriptor, class, quality)?.bitrate;
    if rate == 0 {
        return Some(size);
    }
    let length = descriptor.duration?;
    u64::try_from(u128::from(rate) * length.as_millis() / 1000 / 8).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::AudioStream;
    use spoolframe_types::AudioRole;

    use super::*;

    /// A track: ffprobe's `container` with one stereo `codec` stream at
    /// `bit_rate` over the file.
    fn track(container: &str, codec: &str, bit_rate: u64) -> MediaDescriptor {
        MediaDescriptor {
            container: container.into(),
            duration: Some(Duration::from_secs(240)),
            bit_rate: Some(bit_rate),
            video: None,
            audio: vec![AudioStream {
                index: 0,
                codec: codec.into(),
                profile: None,
                channels: Some(2),
                channel_layout: None,
                language: None,
                title: None,
                default: true,
                role: AudioRole::Main,
            }],
            subtitles: Vec::new(),
        }
    }

    fn flac() -> MediaDescriptor {
        track("flac", "flac", 900_000)
    }

    fn ape() -> MediaDescriptor {
        track("ape", "ape", 800_000)
    }

    fn converts(plan: &MusicPlan) -> Option<&CopyTarget> {
        match &plan.decision {
            Decision::Convert { target, .. } => Some(target),
            _ => None,
        }
    }

    /// A track's expected size: its file's, kept or converted
    /// losslessly; AAC 256 at its rate over its 240 s.
    #[test]
    fn a_track_s_expected_size_is_its_file_s_or_its_aac_s() {
        let size = 27_000_000;
        let expected = |descriptor: &MediaDescriptor, quality| {
            let planned = plan(descriptor, DeviceClass::Apple, quality).unwrap();
            estimate(
                descriptor,
                DeviceClass::Apple,
                quality,
                &planned.decision,
                size,
            )
        };
        assert_eq!(
            [
                expected(&flac(), MusicQuality::Original),
                expected(&ape(), MusicQuality::Original),
                expected(&flac(), MusicQuality::Aac256),
            ],
            [Some(size), Some(size), Some(256_000 / 8 * 240)]
        );
    }

    #[test]
    fn a_flac_is_sent_as_it_is_to_apple_and_linux() {
        for class in [DeviceClass::Apple, DeviceClass::Linux] {
            let plan = plan(&flac(), class, MusicQuality::Original).unwrap();
            assert_eq!(
                (plan.decision, plan.container),
                (Decision::AsIs, Container::Flac)
            );
        }
    }

    #[test]
    fn an_ape_becomes_alac_on_apple() {
        assert_eq!(
            recipe(&ape(), DeviceClass::Apple, MusicQuality::Original).map(|r| r.codec),
            Some(AudioCodec::Alac)
        );
    }

    #[test]
    fn an_ape_becomes_flac_on_linux() {
        let plan = plan(&ape(), DeviceClass::Linux, MusicQuality::Original).unwrap();
        assert_eq!(
            (
                plan.format,
                recipe(&ape(), DeviceClass::Linux, MusicQuality::Original).map(|r| r.codec)
            ),
            (Some(MusicFormat::Flac), Some(AudioCodec::Flac))
        );
    }

    #[test]
    fn an_ape_is_converted_by_the_pool_for_its_class() {
        let plan = plan(&ape(), DeviceClass::Apple, MusicQuality::Original).unwrap();
        assert_eq!(
            converts(&plan),
            Some(&CopyTarget::Music {
                class: DeviceClass::Apple,
                quality: MusicQuality::Original
            })
        );
    }

    #[test]
    fn a_wma_becomes_the_best_aac() {
        let wma = track("asf", "wmav2", 192_000);
        assert_eq!(
            recipe(&wma, DeviceClass::Apple, MusicQuality::Original).map(|r| (r.codec, r.bitrate)),
            Some((AudioCodec::Aac, BEST_AAC))
        );
    }

    #[test]
    fn an_ogg_vorbis_is_converted_on_apple_and_kept_on_linux() {
        let vorbis = track("ogg", "vorbis", 192_000);
        let apple = plan(&vorbis, DeviceClass::Apple, MusicQuality::Original).unwrap();
        let linux = plan(&vorbis, DeviceClass::Linux, MusicQuality::Original).unwrap();
        assert_eq!(
            (converts(&apple).is_some(), linux.decision),
            (true, Decision::AsIs)
        );
    }

    #[test]
    fn aac_in_matroska_is_remuxed_into_mp4_on_apple() {
        let plan = plan(
            &track("matroska,webm", "aac", 256_000),
            DeviceClass::Apple,
            MusicQuality::Original,
        )
        .unwrap();
        assert_eq!(
            (plan.decision, plan.format),
            (Decision::Remux, Some(MusicFormat::Mp4))
        );
    }

    #[test]
    fn aac_256_converts_a_flac() {
        let plan = plan(&flac(), DeviceClass::Linux, MusicQuality::Aac256).unwrap();
        assert_eq!(
            (
                plan.format,
                recipe(&flac(), DeviceClass::Linux, MusicQuality::Aac256)
                    .map(|r| (r.codec, r.bitrate))
            ),
            (Some(MusicFormat::Mp4), Some((AudioCodec::Aac, AAC_256)))
        );
    }

    #[test]
    fn aac_256_sends_a_128_kbit_mp3_as_it_is() {
        let mp3 = track("mp3", "mp3", 128_000);
        assert_eq!(
            plan(&mp3, DeviceClass::Apple, MusicQuality::Aac256)
                .unwrap()
                .decision,
            Decision::AsIs
        );
    }

    #[test]
    fn aac_256_converts_a_320_kbit_mp3() {
        let mp3 = track("mp3", "mp3", 320_000);
        let plan = plan(&mp3, DeviceClass::Apple, MusicQuality::Aac256).unwrap();
        assert!(converts(&plan).is_some(), "{plan:?}");
    }

    #[test]
    fn a_track_with_no_audio_cannot_be_planned() {
        let mut empty = flac();
        empty.audio.clear();
        assert_eq!(
            plan(&empty, DeviceClass::Apple, MusicQuality::Original),
            Err(PlanError::NoAudio(0))
        );
    }

    #[test]
    fn a_quality_s_name_reads_back_as_the_quality() {
        for quality in [MusicQuality::Original, MusicQuality::Aac256] {
            assert_eq!(MusicQuality::parse(quality.as_str()), Some(quality));
        }
    }

    #[test]
    fn dsd_and_pcm_are_lossless_and_mp3_is_not() {
        assert_eq!(
            ["dsd_lsbf_planar", "pcm_s24le", "mp3"].map(lossless),
            [true, true, false]
        );
    }
}
