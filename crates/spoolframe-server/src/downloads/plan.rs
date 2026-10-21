//! What a download is written from (PX-38): the file as it is, a remux
//! of it, or a remux of it with the tracks the pool converted; and how
//! each subtitle picked gets into the file.
//!
//! In original quality (the user's decisions, 2026-10-10 and
//! 2026-10-20):
//!
//! - **As it is**: nothing to convert, the file is already in the
//!   class's container, and "All tracks" is chosen. The file is sent
//!   byte-identical.
//! - **Remux**: nothing to convert, but another container, a track
//!   selection, or subtitles beside the file to carry. Only the chosen
//!   tracks, from the file; nothing is stored.
//! - **Convert**: the device cannot play the video or a chosen audio
//!   track. The pool's `Original` copy for the class converts what the
//!   class cannot play (only that: an Apple copy of Alien's HEVC HDR10
//!   with DTS-HD stores the E-AC-3 alone), and the file is remuxed from
//!   the source and those tracks.
//!
//! A profile's download is always the pool's copy in that profile,
//! remuxed with the chosen tracks.
//!
//! Picture subtitles in an Apple download (MP4 carries no PGS or
//! VobSub): read as text into `mov_text` when the viewer's PX-13 choice
//! is Text and the server reads their language; left out otherwise, and
//! said so. They are never burned in, which would encode the video. A
//! Linux download (Matroska) copies them.

use serde::{Deserialize, Serialize};

use spoolframe_media::ocr::Ocr;
use spoolframe_media::{MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, codecs};
use spoolframe_protocol::downloads::DownloadTracks;
use spoolframe_types::language::same_language;
use spoolframe_types::preferences::ORIGINAL_LANGUAGE;
use spoolframe_types::{
    BitmapPreference, ClientCapabilities, Container, HdrFormat, TrackPreferences, VideoCodec,
    VideoLimits,
};

use super::DeviceClass;
use crate::encode::track_args::kept;
use crate::optimized::mux::UNWRITABLE_TEXT;
use crate::store::optimized::{CopyTarget, CopyVideo, Profile};

/// What a download is made of.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Original quality, converted only where the device needs it.
    Original,
    /// A profile's copy (PX-37), by name.
    Profile(String),
}

/// Which tracks a download carries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tracks {
    /// Every track of the file.
    All,
    /// These, by their position in the file's description: its audio
    /// tracks and its subtitles. The video is always there.
    Chosen {
        audio: Vec<u32>,
        subtitles: Vec<u32>,
    },
}

impl From<Tracks> for DownloadTracks {
    fn from(tracks: Tracks) -> Self {
        match tracks {
            Tracks::All => Self::All,
            Tracks::Chosen { audio, subtitles } => Self::Chosen { audio, subtitles },
        }
    }
}

/// A choice of tracks this server does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownTracks;

impl TryFrom<DownloadTracks> for Tracks {
    type Error = UnknownTracks;

    fn try_from(tracks: DownloadTracks) -> Result<Self, UnknownTracks> {
        match tracks {
            DownloadTracks::All => Ok(Self::All),
            DownloadTracks::Chosen { audio, subtitles } => Ok(Self::Chosen { audio, subtitles }),
            DownloadTracks::Unknown => Err(UnknownTracks),
        }
    }
}

impl Tracks {
    /// The same choice, each list in order and without repeats
    /// ([`DownloadTracks::normalized`]): two requests that differ only so
    /// are one.
    pub fn normalized(&self) -> Self {
        Self::try_from(DownloadTracks::from(self.clone()).normalized())
            .unwrap_or_else(|UnknownTracks| self.clone())
    }

    /// The audio tracks, by position, of `descriptor`.
    pub fn audio(&self, descriptor: &MediaDescriptor) -> Vec<u32> {
        match self {
            Self::All => (0..descriptor.audio.len() as u32).collect(),
            Self::Chosen { audio, .. } => audio.clone(),
        }
    }

    /// The subtitles, by position, of `descriptor`.
    pub fn subtitles(&self, descriptor: &MediaDescriptor) -> Vec<u32> {
        match self {
            Self::All => (0..descriptor.subtitles.len() as u32).collect(),
            Self::Chosen { subtitles, .. } => subtitles.clone(),
        }
    }
}

/// How a download's file is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The media file itself, byte-identical.
    AsIs,
    /// A remux of the chosen tracks from the media file; nothing stored.
    Remux,
    /// A remux from the media file and the pool's copy for `target`,
    /// made with `video`; `audio` are the chosen audio tracks (stream
    /// indexes) the copy converts. The copy may store nothing more than
    /// those (its video copied).
    Convert {
        target: CopyTarget,
        video: CopyVideo,
        audio: Vec<u32>,
    },
}

/// Why a download cannot be planned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("audio track {0}: no such track")]
    NoAudio(u32),
    #[error("subtitle {0}: no such track")]
    NoSubtitle(u32),
    #[error("the file has no video")]
    NoVideo,
    /// The profile's video codec is not one the device plays.
    #[error("this device does not play {0}")]
    NotPlayed(VideoCodec),
}

impl DeviceClass {
    /// The container its downloads are written in.
    pub fn container(self) -> Container {
        match self {
            Self::Apple => Container::Mp4,
            Self::Linux => Container::Matroska,
        }
    }

    /// What a device of this class plays, for a client that sent no
    /// capabilities: Apple's players take H.264 and HEVC with their HDR;
    /// the Linux app plays every codec the server names.
    pub fn capabilities(self) -> ClientCapabilities {
        let video = match self {
            Self::Apple => [VideoCodec::H264, VideoCodec::Hevc].into(),
            Self::Linux => [
                VideoCodec::H264,
                VideoCodec::Hevc,
                VideoCodec::Av1,
                VideoCodec::Vp9,
                VideoCodec::Vp8,
                VideoCodec::Mpeg2,
                VideoCodec::Mpeg4,
                VideoCodec::Vc1,
            ]
            .into(),
        };
        ClientCapabilities {
            video,
            audio: self.keep_audio(),
            containers: [self.container()].into(),
            max_video: Some(VideoLimits {
                hdr: Some([HdrFormat::Hdr10, HdrFormat::Hlg, HdrFormat::DolbyVision].into()),
                ..VideoLimits::default()
            }),
            ..ClientCapabilities::default()
        }
    }
}

/// Check that `tracks` names tracks of `descriptor`.
pub fn check(descriptor: &MediaDescriptor, tracks: &Tracks) -> Result<(), PlanError> {
    if descriptor.video.is_none() {
        return Err(PlanError::NoVideo);
    }
    if let Tracks::Chosen { audio, subtitles } = tracks {
        if let Some(&bad) = audio
            .iter()
            .find(|&&a| a as usize >= descriptor.audio.len())
        {
            return Err(PlanError::NoAudio(bad));
        }
        if let Some(&bad) = subtitles
            .iter()
            .find(|&&s| s as usize >= descriptor.subtitles.len())
        {
            return Err(PlanError::NoSubtitle(bad));
        }
    }
    Ok(())
}

/// How a download of `descriptor`'s file in original quality is written
/// for a `class` device that plays what `caps` say, with `tracks`.
pub fn original(
    descriptor: &MediaDescriptor,
    class: DeviceClass,
    caps: &ClientCapabilities,
    tracks: &Tracks,
) -> Result<Decision, PlanError> {
    check(descriptor, tracks)?;
    let video = descriptor.video.as_ref().ok_or(PlanError::NoVideo)?;
    let keep = class.keep_audio();
    let converted: Vec<u32> = tracks
        .audio(descriptor)
        .iter()
        .map(|&a| &descriptor.audio[a as usize])
        .filter(|a| !kept(codecs::audio_codec(&a.codec), Some(&keep)))
        .map(|a| a.index)
        .collect();
    let video_kept = crate::planner::plays_video(video, caps);
    if video_kept && converted.is_empty() {
        let as_is = *tracks == Tracks::All
            && codecs::container(&descriptor.container) == class.container()
            && !descriptor
                .subtitles
                .iter()
                .any(|s| matches!(s.source, SubtitleSource::External { .. }));
        return Ok(if as_is {
            Decision::AsIs
        } else {
            Decision::Remux
        });
    }
    let codec = codecs::video_codec(&video.codec);
    let copy = if video_kept {
        CopyVideo {
            codec,
            height: video.height,
            max_bitrate: None,
            tone_map: false,
            copied: true,
        }
    } else {
        // At the file's height and bitrate, its HDR kept where the
        // device shows it.
        let to = if caps.video.contains(&VideoCodec::Hevc) {
            VideoCodec::Hevc
        } else {
            VideoCodec::H264
        };
        let height = match caps.max_video.as_ref().and_then(|l| l.height) {
            Some(max) => video.height.min(max),
            None => video.height,
        };
        CopyVideo {
            codec: to,
            height,
            max_bitrate: video.bit_rate.or(descriptor.bit_rate),
            // H.264 carries no HDR a player shows.
            tone_map: crate::planner::tone_maps(video, caps)
                || (video.hdr.is_some() && to == VideoCodec::H264),
            copied: false,
        }
    };
    Ok(Decision::Convert {
        target: CopyTarget::Original { class },
        video: copy,
        audio: converted,
    })
}

/// Whether a device that plays `caps` plays `profile`'s copies: what
/// download options list.
pub fn plays(caps: &ClientCapabilities, profile: &Profile) -> bool {
    caps.video.contains(&profile.video_codec)
}

/// How a download of `descriptor`'s file in `profile` is written, with
/// `tracks`: always from the profile's copy, refused when a device that
/// plays `caps` would not play it.
pub fn profile(
    descriptor: &MediaDescriptor,
    profile: &Profile,
    caps: &ClientCapabilities,
    tracks: &Tracks,
) -> Result<Decision, PlanError> {
    check(descriptor, tracks)?;
    if !plays(caps, profile) {
        return Err(PlanError::NotPlayed(profile.video_codec));
    }
    let video = descriptor.video.as_ref().ok_or(PlanError::NoVideo)?;
    let audio = tracks
        .audio(descriptor)
        .iter()
        .map(|&a| &descriptor.audio[a as usize])
        .filter(|a| !kept(codecs::audio_codec(&a.codec), None))
        .map(|a| a.index)
        .collect();
    Ok(Decision::Convert {
        target: CopyTarget::Profile(profile.name.clone()),
        video: CopyVideo::of_profile(profile, video.height, video.hdr.is_some()),
        audio,
    })
}

/// The choice a download's options show first (PX-38): the audio track
/// playback would pick for the viewer's `preferences` (and `override_`),
/// and one more in another of their audio languages when the file has
/// one; every text subtitle in their subtitle languages (their audio
/// languages when they set none), and the subtitle playback would pick.
pub fn preferred(
    descriptor: &MediaDescriptor,
    preferences: &TrackPreferences,
    override_: Option<&crate::tracks::TrackOverride>,
) -> Tracks {
    let tracks = descriptor.tracks();
    let chosen = crate::tracks::choose(&tracks, preferences, override_).selection;
    let original = tracks
        .audio
        .iter()
        .find(|a| a.default)
        .or(tracks.audio.first())
        .and_then(|a| a.language.clone());
    let resolve = |wanted: &String| {
        if wanted == ORIGINAL_LANGUAGE {
            original.clone()
        } else {
            Some(wanted.clone())
        }
    };
    let audio_languages: Vec<String> = preferences
        .audio
        .languages
        .iter()
        .filter_map(resolve)
        .collect();
    let mut audio: Vec<u32> = chosen.audio.into_iter().collect();
    let first_language = chosen
        .audio
        .and_then(|a| tracks.audio.get(a as usize))
        .and_then(|a| a.language.clone());
    let second = audio_languages
        .iter()
        .filter(|l| {
            !first_language
                .as_deref()
                .is_some_and(|first| same_language(first, l))
        })
        .find_map(|language| {
            (0..tracks.audio.len())
                .filter(|&i| {
                    let a = &tracks.audio[i];
                    !preferences.audio.avoid.contains(&a.role)
                        && a.language
                            .as_deref()
                            .is_some_and(|l| same_language(l, language))
                })
                .min_by_key(|&i| {
                    let a = &tracks.audio[i];
                    (!a.default, std::cmp::Reverse(a.channels.unwrap_or(0)), i)
                })
        });
    audio.extend(second.map(|i| i as u32));
    let subtitle_languages = if preferences.subtitles.languages.is_empty() {
        audio_languages
    } else {
        preferences.subtitles.languages.clone()
    };
    let mut subtitles: Vec<u32> = (0..descriptor.subtitles.len() as u32)
        .filter(|&i| {
            let stream = &descriptor.subtitles[i as usize];
            stream.kind == SubtitleKind::Text
                && stream.language.as_deref().is_some_and(|l| {
                    subtitle_languages
                        .iter()
                        .any(|wanted| same_language(l, wanted))
                })
        })
        .collect();
    subtitles.extend(chosen.subtitle);
    Tracks::Chosen { audio, subtitles }.normalized()
}

/// How a subtitle picked for a download gets into its file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subtitle {
    /// Copied as it is (Matroska), or written as `mov_text` (MP4).
    Written,
    /// Pictures read as text (OCR, PX-13's Text), then as `mov_text`.
    ReadAsText,
    /// Not in the file, and why: said to the user.
    LeftOut(String),
}

/// `decision` once the subtitles' `ways` are known: the file as it is
/// carries every track as it is, so it is sent so only when every
/// subtitle is written as it is. One the class leaves out, or reads as
/// text, makes it a remux: what the download says it carries is then
/// what its file holds.
pub fn settled(decision: Decision, ways: &[(u32, Subtitle)]) -> Decision {
    if decision == Decision::AsIs && ways.iter().any(|(_, way)| *way != Subtitle::Written) {
        Decision::Remux
    } else {
        decision
    }
}

/// Each of `picked` (subtitle positions in `descriptor`) as a `class`
/// download carries it, the viewer preferring `bitmap` for pictures and
/// the server reading text with `ocr`.
pub fn subtitles(
    descriptor: &MediaDescriptor,
    class: DeviceClass,
    picked: &[u32],
    bitmap: BitmapPreference,
    ocr: &Ocr,
) -> Vec<(u32, Subtitle)> {
    picked
        .iter()
        .filter_map(|&at| {
            let stream = descriptor.subtitles.get(at as usize)?;
            Some((at, subtitle(stream, class, bitmap, ocr)))
        })
        .collect()
}

fn subtitle(
    stream: &SubtitleStream,
    class: DeviceClass,
    bitmap: BitmapPreference,
    ocr: &Ocr,
) -> Subtitle {
    if UNWRITABLE_TEXT.contains(&stream.codec.as_str())
        || codecs::subtitle_format(&stream.codec) == spoolframe_types::SubtitleFormat::Unknown
    {
        return Subtitle::LeftOut(format!("{} cannot be written to a file", stream.codec));
    }
    match (class, stream.kind) {
        (DeviceClass::Linux, _) | (DeviceClass::Apple, SubtitleKind::Text) => Subtitle::Written,
        (DeviceClass::Apple, SubtitleKind::Bitmap) => match bitmap {
            BitmapPreference::Pictures => Subtitle::LeftOut(
                "picture subtitles are not carried in an MP4 file; with Text chosen for picture \
                 subtitles they are read as text"
                    .into(),
            ),
            BitmapPreference::Text => match ocr.language_for(stream.language.as_deref()) {
                Some(_) => Subtitle::ReadAsText,
                None => Subtitle::LeftOut(
                    "picture subtitles are not carried in an MP4 file, and the server cannot \
                     read this language as text"
                        .into(),
                ),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::{AudioStream, VideoStream};
    use spoolframe_types::{AudioRole, SubtitleRole};

    use super::*;

    fn audio(index: u32, codec: &str) -> AudioStream {
        AudioStream {
            index,
            codec: codec.into(),
            profile: None,
            channels: Some(6),
            channel_layout: None,
            language: Some("eng".into()),
            title: None,
            default: index == 1,
            role: AudioRole::Main,
        }
    }

    fn sub(index: u32, codec: &str, language: &str) -> SubtitleStream {
        SubtitleStream {
            source: SubtitleSource::Embedded { index },
            kind: SubtitleKind::of_codec(codec),
            codec: codec.into(),
            language: Some(language.into()),
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
        }
    }

    /// A 1080p film in `container` with `video` and `audio` tracks (one
    /// stream each, from 1), and a SubRip and a PGS subtitle after them.
    fn film(container: &str, video: &str, audio_codecs: &[&str]) -> MediaDescriptor {
        let after = audio_codecs.len() as u32 + 1;
        MediaDescriptor {
            container: container.into(),
            duration: Some(Duration::from_secs(60)),
            bit_rate: Some(8_000_000),
            video: Some(VideoStream {
                index: 0,
                codec: video.into(),
                profile: None,
                level: None,
                width: 1920,
                height: 1080,
                pixel_format: None,
                color_transfer: None,
                frame_rate_millis: None,
                bit_depth: 8,
                hdr: None,
                dolby_vision: None,
                bit_rate: None,
                parameter_sets_in_record: true,
            }),
            audio: audio_codecs
                .iter()
                .enumerate()
                .map(|(i, c)| audio(i as u32 + 1, c))
                .collect(),
            subtitles: vec![
                sub(after, "subrip", "eng"),
                sub(after + 1, "hdmv_pgs_subtitle", "eng"),
            ],
        }
    }

    const MP4: &str = "mov,mp4,m4a,3gp,3g2,mj2";
    const MKV: &str = "matroska,webm";

    fn apple(descriptor: &MediaDescriptor, tracks: &Tracks) -> Decision {
        let class = DeviceClass::Apple;
        original(descriptor, class, &class.capabilities(), tracks).unwrap()
    }

    fn linux(descriptor: &MediaDescriptor, tracks: &Tracks) -> Decision {
        let class = DeviceClass::Linux;
        original(descriptor, class, &class.capabilities(), tracks).unwrap()
    }

    /// An MP4 of H.264 and AAC with its text subtitle alone (MP4 holds no
    /// PGS).
    fn h264_aac_mp4() -> MediaDescriptor {
        let mut d = film(MP4, "h264", &["aac"]);
        d.subtitles = vec![sub(2, "mov_text", "eng")];
        d
    }

    fn first_audio() -> Tracks {
        Tracks::Chosen {
            audio: vec![0],
            subtitles: Vec::new(),
        }
    }

    #[test]
    fn an_mp4_an_apple_device_plays_with_all_tracks_is_sent_as_it_is() {
        assert_eq!(apple(&h264_aac_mp4(), &Tracks::All), Decision::AsIs);
    }

    #[test]
    fn a_track_selection_of_a_file_played_as_it_is_is_a_remux() {
        assert_eq!(apple(&h264_aac_mp4(), &first_audio()), Decision::Remux);
    }

    #[test]
    fn a_matroska_file_an_apple_device_plays_is_a_remux() {
        assert_eq!(
            apple(&film(MKV, "h264", &["aac"]), &Tracks::All),
            Decision::Remux
        );
    }

    #[test]
    fn a_file_with_subtitles_beside_it_is_a_remux_with_all_tracks() {
        let mut d = h264_aac_mp4();
        d.subtitles[0].source = SubtitleSource::External {
            path: "/films/a.srt".into(),
        };
        d.subtitles[0].codec = "subrip".into();
        assert_eq!(apple(&d, &Tracks::All), Decision::Remux);
    }

    /// Alien on an iPhone: its HEVC HDR10 kept, its DTS made E-AC-3.
    #[test]
    fn hevc_hdr10_with_dts_on_apple_converts_only_the_dts() {
        let mut d = film(MKV, "hevc", &["dts"]);
        let video = d.video.as_mut().unwrap();
        video.bit_depth = 10;
        video.hdr = Some(HdrFormat::Hdr10);
        assert_eq!(
            apple(&d, &Tracks::All),
            Decision::Convert {
                target: CopyTarget::Original {
                    class: DeviceClass::Apple
                },
                video: CopyVideo {
                    codec: VideoCodec::Hevc,
                    height: 1080,
                    max_bitrate: None,
                    tone_map: false,
                    copied: true,
                },
                audio: vec![1],
            }
        );
    }

    #[test]
    fn dts_on_linux_is_kept_in_a_remux() {
        assert_eq!(
            linux(&film(MKV, "h264", &["dts"]), &first_audio()),
            Decision::Remux
        );
    }

    #[test]
    fn a_dts_track_not_chosen_converts_nothing() {
        assert_eq!(
            apple(
                &film(MKV, "h264", &["ac3", "dts"]),
                &Tracks::Chosen {
                    audio: vec![0],
                    subtitles: vec![0]
                }
            ),
            Decision::Remux
        );
    }

    #[test]
    fn a_video_an_apple_device_cannot_play_is_converted_at_its_height() {
        let decision = apple(&film(MKV, "vp9", &["aac"]), &Tracks::All);
        assert!(
            matches!(
                decision,
                Decision::Convert {
                    video: CopyVideo {
                        codec: VideoCodec::Hevc,
                        height: 1080,
                        max_bitrate: Some(8_000_000),
                        copied: false,
                        ..
                    },
                    ..
                }
            ),
            "{decision:?}"
        );
    }

    #[test]
    fn a_profile_download_converts_in_the_profile() {
        let mobile = Profile {
            name: "Mobile".into(),
            video_codec: VideoCodec::H264,
            height: 720,
            max_bitrate: 4_000_000,
        };
        let caps = DeviceClass::Apple.capabilities();
        assert_eq!(
            profile(&h264_aac_mp4(), &mobile, &caps, &Tracks::All).unwrap(),
            Decision::Convert {
                target: CopyTarget::Profile("Mobile".into()),
                video: CopyVideo {
                    codec: VideoCodec::H264,
                    height: 720,
                    max_bitrate: Some(4_000_000),
                    tone_map: false,
                    copied: false,
                },
                audio: Vec::new(),
            }
        );
    }

    #[test]
    fn a_profile_the_device_does_not_play_is_refused() {
        let tv = Profile {
            name: "TV".into(),
            video_codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: 12_000_000,
        };
        let caps = ClientCapabilities {
            video: [VideoCodec::H264].into(),
            ..DeviceClass::Apple.capabilities()
        };
        assert_eq!(
            profile(&h264_aac_mp4(), &tv, &caps, &Tracks::All),
            Err(PlanError::NotPlayed(VideoCodec::Hevc))
        );
    }

    #[test]
    fn a_track_that_is_not_there_is_refused() {
        let tracks = Tracks::Chosen {
            audio: vec![5],
            subtitles: Vec::new(),
        };
        let class = DeviceClass::Apple;
        assert_eq!(
            original(&h264_aac_mp4(), class, &class.capabilities(), &tracks),
            Err(PlanError::NoAudio(5))
        );
    }

    #[test]
    fn a_choice_in_another_order_is_the_same_choice() {
        let a = Tracks::Chosen {
            audio: vec![2, 0, 2],
            subtitles: vec![1, 0],
        };
        let b = Tracks::Chosen {
            audio: vec![0, 2],
            subtitles: vec![0, 1],
        };
        assert_eq!(a.normalized(), b.normalized());
    }

    fn ocr_eng() -> Ocr {
        Ocr::new(
            spoolframe_media::sandbox::Tool::unconfined("/nonexistent/tesseract"),
            ["eng".into()],
        )
    }

    fn ways(class: DeviceClass, bitmap: BitmapPreference, language: &str) -> Vec<(u32, Subtitle)> {
        let mut d = film(MKV, "h264", &["aac"]);
        d.subtitles[1].language = Some(language.into());
        subtitles(&d, class, &[0, 1], bitmap, &ocr_eng())
    }

    fn left_out(way: &Subtitle) -> bool {
        matches!(way, Subtitle::LeftOut(_))
    }

    #[test]
    fn apple_reads_picture_subtitles_as_text_when_text_is_chosen() {
        assert_eq!(
            ways(DeviceClass::Apple, BitmapPreference::Text, "eng"),
            [(0, Subtitle::Written), (1, Subtitle::ReadAsText)]
        );
    }

    #[test]
    fn apple_leaves_picture_subtitles_out_when_pictures_are_chosen() {
        let ways = ways(DeviceClass::Apple, BitmapPreference::Pictures, "eng");
        assert!(left_out(&ways[1].1), "{ways:?}");
    }

    #[test]
    fn apple_leaves_picture_subtitles_out_in_a_language_it_cannot_read() {
        let ways = ways(DeviceClass::Apple, BitmapPreference::Text, "jpn");
        assert!(left_out(&ways[1].1), "{ways:?}");
    }

    /// Finding 3 of the 0.14.0 review: an MP4 an Apple device plays with
    /// every track, but with a picture subtitle it leaves out, is not sent
    /// as it is: the file would carry the subtitle the download says it
    /// left out.
    #[test]
    fn a_file_with_a_subtitle_left_out_is_not_sent_as_it_is() {
        let ways = ways(DeviceClass::Apple, BitmapPreference::Pictures, "eng");
        assert_eq!(settled(Decision::AsIs, &ways), Decision::Remux);
    }

    #[test]
    fn a_file_with_every_subtitle_written_is_sent_as_it_is() {
        let ways = [(0, Subtitle::Written)];
        assert_eq!(settled(Decision::AsIs, &ways), Decision::AsIs);
    }

    #[test]
    fn linux_copies_picture_subtitles() {
        assert_eq!(
            ways(DeviceClass::Linux, BitmapPreference::Pictures, "eng"),
            [(0, Subtitle::Written), (1, Subtitle::Written)]
        );
    }

    /// English and French audio, English and French SubRip, an English
    /// PGS and a German SubRip.
    fn bilingual() -> MediaDescriptor {
        let mut d = film(MKV, "h264", &["aac", "ac3", "aac"]);
        d.audio[1].language = Some("fre".into());
        d.audio[2].role = AudioRole::Commentary;
        d.subtitles = vec![
            sub(4, "subrip", "eng"),
            sub(5, "hdmv_pgs_subtitle", "eng"),
            sub(6, "subrip", "fre"),
            sub(7, "subrip", "ger"),
        ];
        d
    }

    fn preferring(audio: &[&str], subtitles: &[&str]) -> TrackPreferences {
        let mut p = TrackPreferences::default();
        p.audio.languages = audio.iter().map(|l| l.to_string()).collect();
        p.subtitles.languages = subtitles.iter().map(|l| l.to_string()).collect();
        p
    }

    #[test]
    fn the_preferred_choice_is_two_audio_languages_and_their_text_subtitles() {
        assert_eq!(
            preferred(&bilingual(), &preferring(&["en", "fr"], &[]), None),
            Tracks::Chosen {
                audio: vec![0, 1],
                subtitles: vec![0, 2],
            }
        );
    }

    #[test]
    fn the_preferred_subtitles_follow_the_subtitle_languages_when_set() {
        assert_eq!(
            preferred(&bilingual(), &preferring(&["en"], &["de"]), None),
            Tracks::Chosen {
                audio: vec![0],
                subtitles: vec![3],
            }
        );
    }
}
