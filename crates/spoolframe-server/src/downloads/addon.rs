//! Download add-ons (feature `spoolframe:download-addons`): what a
//! device may add to a film's download it has, and how an audio track's
//! own file is written.
//!
//! An audio track is a file of its own, an M4A on Apple (AVFoundation
//! merges an audio-only M4A into the film's movie) and a Matroska audio
//! file on Linux: the track as it is when the class keeps its codec
//! ([`DeviceClass::keep_audio`]), else the pool's `Original` copy for the
//! class converts it, as it does for a film's download, so the two share
//! the converted track. Its video is copied: an add-on never encodes the
//! picture. The file keeps the film's timestamps, as the film's download
//! does, so the two line up.
//!
//! A subtitle is read as WebVTT (the client's `ReadTrackSubtitle`); one
//! the server cannot make text of is left out, and said why.

use spoolframe_media::ocr::Ocr;
use spoolframe_media::{MediaDescriptor, SubtitleKind, SubtitleStream, codecs};
use spoolframe_types::AudioCodec;

use super::DeviceClass;
use super::music::{MusicFormat, NO_VIDEO};
use super::plan::{Decision, PlanError};
use crate::optimized::mux::UNWRITABLE_TEXT;
use crate::planner::{MAX_ENCODED_CHANNELS, encoded_audio_bitrate};
use crate::store::optimized::{CopyTarget, CopyVideo};

/// How an audio add-on's file is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPlan {
    /// A remux of the track from the film, or from the pool's copy.
    pub decision: Decision,
    pub format: MusicFormat,
}

/// The container a `class` device's audio add-on is written in.
pub fn format(class: DeviceClass) -> MusicFormat {
    match class {
        DeviceClass::Apple => MusicFormat::Mp4,
        DeviceClass::Linux => MusicFormat::Matroska,
    }
}

/// How audio track `place` (its place in the descriptor) of
/// `descriptor`'s film is written as a `class` device's add-on.
pub fn audio(
    descriptor: &MediaDescriptor,
    class: DeviceClass,
    place: u32,
) -> Result<AudioPlan, PlanError> {
    let track = descriptor
        .audio
        .get(place as usize)
        .ok_or(PlanError::NoAudio(place))?;
    let format = format(class);
    if class
        .keep_audio()
        .contains(&codecs::audio_codec(&track.codec))
    {
        return Ok(AudioPlan {
            decision: Decision::Remux,
            format,
        });
    }
    // The video as a film's download that keeps it asks the copy: the
    // same copy, so its converted tracks are shared.
    let video = descriptor.video.as_ref().map_or(NO_VIDEO, |v| CopyVideo {
        codec: codecs::video_codec(&v.codec),
        height: v.height,
        max_bitrate: None,
        tone_map: false,
        copied: true,
    });
    Ok(AudioPlan {
        decision: Decision::Convert {
            target: CopyTarget::Original { class },
            video,
            audio: vec![track.index],
        },
        format,
    })
}

/// About how many bytes audio track `place` of `descriptor`'s film is as
/// an add-on written as `decision`: a converted track's rate, or a lossy
/// codec's usual one, over the film's length. `None` for a lossless track
/// kept as it is, and when the length is not known.
pub fn estimate(descriptor: &MediaDescriptor, place: u32, decision: &Decision) -> Option<u64> {
    let track = descriptor.audio.get(place as usize)?;
    let channels = track.channels.unwrap_or(2).clamp(1, MAX_ENCODED_CHANNELS);
    let rate = match decision {
        Decision::Convert { .. } => encoded_audio_bitrate(AudioCodec::Eac3, channels),
        _ => match codecs::audio_codec(&track.codec) {
            codec @ (AudioCodec::Aac
            | AudioCodec::Ac3
            | AudioCodec::Eac3
            | AudioCodec::Mp3
            | AudioCodec::Opus
            | AudioCodec::Vorbis) => encoded_audio_bitrate(codec, channels),
            _ => return None,
        },
    };
    let length = descriptor.duration?;
    super::bytes_at(rate, length)
}

/// Why subtitle `stream` cannot be read as WebVTT, said to the user; `None`
/// when it can: text the server converts, or pictures in a language
/// `ocr` reads.
pub fn subtitle_left_out(stream: &SubtitleStream, ocr: &Ocr) -> Option<String> {
    if UNWRITABLE_TEXT.contains(&stream.codec.as_str())
        || codecs::subtitle_format(&stream.codec) == spoolframe_types::SubtitleFormat::Unknown
    {
        return Some(format!("{} cannot be read as text", stream.codec));
    }
    match stream.kind {
        SubtitleKind::Text => None,
        SubtitleKind::Bitmap => ocr
            .language_for(stream.language.as_deref())
            .is_none()
            .then(|| "the server cannot read this language's picture subtitles as text".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::sandbox::Tool;
    use spoolframe_media::{AudioStream, SubtitleSource, VideoStream};
    use spoolframe_types::{AudioRole, SubtitleRole};

    use super::*;

    fn audio_track(index: u32, codec: &str, channels: u32) -> AudioStream {
        AudioStream {
            index,
            codec: codec.into(),
            profile: None,
            channels: Some(channels),
            channel_layout: None,
            language: Some("fre".into()),
            title: None,
            default: false,
            role: AudioRole::Main,
        }
    }

    /// A 100 s H.264 film: stream 1 DTS 5.1, stream 2 AAC stereo, stream
    /// 3 FLAC.
    fn film() -> MediaDescriptor {
        MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(100)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "h264".into(),
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
                codec_string: None,
            }),
            audio: vec![
                audio_track(1, "dts", 6),
                audio_track(2, "aac", 2),
                audio_track(3, "flac", 2),
            ],
            subtitles: Vec::new(),
        }
    }

    #[test]
    fn aac_is_copied_into_an_m4a_on_apple() {
        assert_eq!(
            audio(&film(), DeviceClass::Apple, 1),
            Ok(AudioPlan {
                decision: Decision::Remux,
                format: MusicFormat::Mp4
            })
        );
    }

    /// DTS is converted by the film's own copy for Apple, its video copied.
    #[test]
    fn dts_is_converted_by_the_class_s_copy_on_apple() {
        let plan = audio(&film(), DeviceClass::Apple, 0).unwrap();
        let Decision::Convert {
            target,
            video,
            audio,
        } = plan.decision
        else {
            panic!("{plan:?}");
        };
        assert_eq!(
            (target, video.copied, audio),
            (
                CopyTarget::Original {
                    class: DeviceClass::Apple
                },
                true,
                vec![1]
            )
        );
    }

    #[test]
    fn dts_is_copied_into_matroska_on_linux() {
        assert_eq!(
            audio(&film(), DeviceClass::Linux, 0),
            Ok(AudioPlan {
                decision: Decision::Remux,
                format: MusicFormat::Matroska
            })
        );
    }

    #[test]
    fn a_track_the_film_has_not_is_refused() {
        assert_eq!(
            audio(&film(), DeviceClass::Apple, 3),
            Err(PlanError::NoAudio(3))
        );
    }

    /// AAC stereo at its usual 192 kbit/s, E-AC-3 5.1 at 640 kbit/s, over
    /// 100 s; FLAC kept as it is cannot be told.
    #[test]
    fn an_add_on_s_size_is_its_rate_over_the_film_s_length() {
        let film = film();
        let size = |place| {
            let plan = audio(&film, DeviceClass::Apple, place).unwrap();
            estimate(&film, place, &plan.decision)
        };
        assert_eq!(
            [size(1), size(0), size(2)],
            [Some(192_000 / 8 * 100), Some(640_000 / 8 * 100), None]
        );
    }

    fn subtitle(codec: &str, kind: SubtitleKind, language: &str) -> SubtitleStream {
        SubtitleStream {
            source: SubtitleSource::Embedded { index: 4 },
            codec: codec.into(),
            kind,
            language: Some(language.into()),
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
        }
    }

    #[test]
    fn teletext_and_pictures_the_server_cannot_read_are_left_out() {
        let ocr = Ocr::new(Tool::unconfined("/nonexistent/tesseract"), ["eng".into()]);
        let left_out = |codec, kind, language| {
            subtitle_left_out(&subtitle(codec, kind, language), &ocr).is_some()
        };
        assert_eq!(
            [
                left_out("subrip", SubtitleKind::Text, "jpn"),
                left_out("dvb_teletext", SubtitleKind::Text, "eng"),
                left_out("hdmv_pgs_subtitle", SubtitleKind::Bitmap, "eng"),
                left_out("hdmv_pgs_subtitle", SubtitleKind::Bitmap, "jpn"),
            ],
            [false, true, false, true]
        );
    }
}
