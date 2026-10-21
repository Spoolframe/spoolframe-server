//! A copy's tracks (PX-37's track store, the user's decision 2026-10-20):
//! which streams of a file a copy stores, and the ffmpeg command line
//! that makes each one, a Matroska file of its own.
//!
//! A copy stores its video converted as its profile (or the playback it
//! is made ahead for) says, unless it is copied (PX-38), and each audio
//! track it does not keep as it is, converted. Everything else is read
//! from the file whenever the copy is: kept tracks, subtitles, chapters,
//! attachments, metadata.
//!
//! A track keeps the file's timestamps (`-copyts`), so it lines up with
//! the file's other streams when the two are read together, a late
//! audio track's start and an encoder's priming included. It is written
//! bit-exact, with no metadata of its own: made again after an eviction,
//! it is the same bytes (PX-38's downloads keep a stable SHA-256).

use std::collections::BTreeSet;

use spoolframe_media::encoders::Encoders;
use spoolframe_media::{MediaDescriptor, codecs};
use spoolframe_types::AudioCodec;

use super::args::{
    BITEXACT_ARGS, EncodeSpec, Output, PROGRESS_ARGS, QUIET_ARGS, input_video_args, s,
};
use crate::planner::{MAX_ENCODED_CHANNELS, encoded_audio_bitrate};
use crate::store::optimized::{AudioRecipe, CopyVideo, Recipe};

/// The audio codecs a copy keeps as they are unless it says otherwise
/// (PX-37): a client plays them as they are.
pub(crate) const KEPT_AUDIO: [AudioCodec; 3] = [AudioCodec::Aac, AudioCodec::Ac3, AudioCodec::Eac3];

/// What every track's output is written with, beside [`BITEXACT_ARGS`]:
/// no metadata of the file's, so a track made twice is the same bytes.
const NO_METADATA: [&str; 4] = ["-map_metadata", "-1", "-map_chapters", "-1"];

/// Whether audio in `codec` is kept: it is in `keep`, the copy's own set
/// (PX-38), or in [`KEPT_AUDIO`] for a copy without one.
pub(crate) fn kept(codec: AudioCodec, keep: Option<&BTreeSet<AudioCodec>>) -> bool {
    match keep {
        Some(keep) => keep.contains(&codec),
        None => KEPT_AUDIO.contains(&codec),
    }
}

/// The tracks a copy of `descriptor`'s file made as `video`, keeping
/// `keep`, stores: its video unless copied, then each audio track it does
/// not keep, converted to E-AC-3, or AAC where `encoders` have no E-AC-3,
/// at a playback's channels and bitrate for it. The error says why there
/// are none: audio to convert and no encoder for it.
pub(crate) fn recipes(
    descriptor: &MediaDescriptor,
    video: CopyVideo,
    keep: Option<&BTreeSet<AudioCodec>>,
    encoders: &Encoders,
) -> Result<Vec<Recipe>, String> {
    let mut recipes = Vec::new();
    if !video.copied && descriptor.video.is_some() {
        recipes.push(Recipe::Video(video));
    }
    for track in &descriptor.audio {
        if kept(codecs::audio_codec(&track.codec), keep) {
            continue;
        }
        let codec = [AudioCodec::Eac3, AudioCodec::Aac]
            .into_iter()
            .find(|&codec| encoders.audio_encoder(codec).is_some())
            .ok_or_else(|| format!("audio {}: no encoder for eac3 or aac", track.index))?;
        let channels = track.channels.unwrap_or(2).clamp(1, MAX_ENCODED_CHANNELS);
        recipes.push(Recipe::Audio(AudioRecipe {
            stream: track.index,
            codec,
            channels,
            bitrate: encoded_audio_bitrate(codec, channels),
        }));
    }
    Ok(recipes)
}

/// ffmpeg's arguments for `recipe`'s track of `spec.descriptor`'s file,
/// written to `spec.output`'s file (`Output::Progressive`, the track's
/// `.part`). For a video track `spec.video` is the recipe's
/// (`CopyVideo` into `ConvertedVideo`); its audio, subtitle and sidecar
/// are not a track's and are refused. The error says why there are no
/// arguments: no encoder for it.
pub(crate) fn track_args(spec: &EncodeSpec<'_>, recipe: &Recipe) -> Result<Vec<String>, String> {
    let Output::Progressive { file } = &spec.output else {
        return Err("a track is one Matroska file".into());
    };
    if spec.audio.is_some() || spec.subtitle.is_some() || spec.inputs.sidecar.is_some() {
        return Err("a track is one stream of its file, not a plan's".into());
    }
    let mut args = match recipe {
        // The video's part is a conversion's own: the same decode, scale
        // and tone map as a single encode of that plan (PGPU-04).
        Recipe::Video(_) => input_video_args(spec)?,
        Recipe::Audio(audio) => {
            let encoder = match audio.codec {
                // A track's lossless copy (PX-51): ffmpeg's own encoders,
                // in every build, and not ones playback picks from.
                AudioCodec::Alac => s("alac"),
                AudioCodec::Flac => s("flac"),
                codec => spec
                    .encoders
                    .audio_encoder(codec)
                    .ok_or_else(|| format!("no encoder for {codec} on this machine"))?
                    .name
                    .clone(),
            };
            let mut args: Vec<String> = QUIET_ARGS.map(s).to_vec();
            args.extend([s("-i"), s(spec.inputs.film.display())]);
            args.extend([
                s("-map"),
                format!("0:{}", audio.stream),
                s("-c:a"),
                encoder,
                s("-ac"),
                s(audio.channels),
            ]);
            // Lossless has no rate to aim at.
            if audio.bitrate > 0 {
                args.extend([s("-b:a"), s(audio.bitrate)]);
            }
            args
        }
    };
    // The file's own times, as the file's other streams have them.
    args.push(s("-copyts"));
    args.extend(NO_METADATA.map(s));
    args.extend(BITEXACT_ARGS.map(s));
    args.extend(PROGRESS_ARGS.map(s));
    // Not `-live`: the muxer goes back to write the cues and sizes, so the
    // track seeks anywhere at once. Nobody reads it before it is whole.
    args.extend([s("-f"), s("matroska"), file.display().to_string()]);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use spoolframe_media::encoders::{AudioEncoder, ToneMapSetting, VideoEncoder};
    use spoolframe_media::{AudioStream, MediaDescriptor, VideoStream};
    use spoolframe_types::{AudioRole, HdrFormat, VideoCodec};

    use super::*;
    use crate::downloads::DeviceClass;
    use crate::encode::args::Inputs;

    /// A 4K HDR10 HEVC MKV: stream 0 video, 1 DTS 5.1, 2 AC-3 5.1.
    fn film() -> MediaDescriptor {
        let audio = |index, codec: &str| AudioStream {
            index,
            codec: codec.into(),
            profile: None,
            channels: Some(6),
            channel_layout: None,
            language: Some("eng".into()),
            title: None,
            default: index == 1,
            role: AudioRole::Main,
        };
        MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(7200)),
            bit_rate: Some(40_000_000),
            video: Some(VideoStream {
                index: 0,
                codec: "hevc".into(),
                profile: Some("Main 10".into()),
                level: Some(153),
                width: 3840,
                height: 2160,
                pixel_format: Some("yuv420p10le".into()),
                color_transfer: Some("smpte2084".into()),
                frame_rate_millis: Some(23_976),
                bit_depth: 10,
                hdr: Some(HdrFormat::Hdr10),
                dolby_vision: None,
                bit_rate: None,
                parameter_sets_in_record: false,
            }),
            audio: vec![audio(1, "dts"), audio(2, "ac3")],
            subtitles: Vec::new(),
        }
    }

    fn encoders() -> Encoders {
        Encoders {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "libx264".into(),
                hardware: None,
            }],
            audio: vec![
                AudioEncoder {
                    codec: AudioCodec::Aac,
                    name: "aac".into(),
                },
                AudioEncoder {
                    codec: AudioCodec::Eac3,
                    name: "eac3".into(),
                },
            ],
            ..Encoders::software_baseline()
        }
    }

    const MOBILE: CopyVideo = CopyVideo {
        codec: VideoCodec::H264,
        height: 1080,
        max_bitrate: Some(4_000_000),
        tone_map: false,
        copied: false,
    };

    /// The film's own video, copied (PX-38).
    const COPIED: CopyVideo = CopyVideo {
        codec: VideoCodec::Hevc,
        height: 2160,
        max_bitrate: None,
        tone_map: false,
        copied: true,
    };

    /// The DTS track as E-AC-3 5.1 at a playback's rate.
    const DTS_AS_EAC3: Recipe = Recipe::Audio(AudioRecipe {
        stream: 1,
        codec: AudioCodec::Eac3,
        channels: 6,
        bitrate: 640_000,
    });

    fn args(recipe: Recipe) -> Vec<String> {
        let video = match recipe {
            Recipe::Video(video) => Some(video.into()),
            Recipe::Audio(_) => None,
        };
        let encoders = encoders();
        track_args(
            &EncodeSpec {
                inputs: Inputs::film(Path::new("/optimized/making/t1.input")),
                descriptor: &film(),
                video,
                audio: None,
                subtitle: None,
                encoders: &encoders,
                vaapi_device: Path::new(""),
                hardware_decode: true,
                gpu_filters: false,
                tone_map: ToneMapSetting::Auto,
                start: None,
                output: Output::Progressive {
                    file: "/optimized/making/t1.mkv.part".into(),
                },
            },
            &recipe,
        )
        .unwrap()
    }

    /// Whether `args` hold `run`, one after the other.
    fn has(args: &[String], run: &[&str]) -> bool {
        args.windows(run.len()).any(|w| w == run)
    }

    #[test]
    fn a_profile_copy_stores_its_video_and_its_dts_as_e_ac_3() {
        assert_eq!(
            recipes(&film(), MOBILE, None, &encoders()).unwrap(),
            [Recipe::Video(MOBILE), DTS_AS_EAC3]
        );
    }

    #[test]
    fn an_apple_copy_with_its_video_copied_stores_only_its_dts_as_e_ac_3() {
        let keep = DeviceClass::Apple.keep_audio();
        assert_eq!(
            recipes(&film(), COPIED, Some(&keep), &encoders()).unwrap(),
            [DTS_AS_EAC3]
        );
    }

    #[test]
    fn a_linux_copy_with_its_video_copied_stores_nothing() {
        let keep = DeviceClass::Linux.keep_audio();
        assert!(
            recipes(&film(), COPIED, Some(&keep), &encoders())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn without_an_e_ac_3_encoder_dts_is_stored_as_aac() {
        let aac_only = Encoders::software_baseline();
        assert_eq!(
            recipes(&film(), COPIED, None, &aac_only).unwrap(),
            [Recipe::Audio(AudioRecipe {
                stream: 1,
                codec: AudioCodec::Aac,
                channels: 6,
                bitrate: 384_000,
            })]
        );
    }

    #[test]
    fn an_audio_track_maps_its_stream_alone_and_encodes_it() {
        assert!(has(
            &args(DTS_AS_EAC3),
            &["-map", "0:1", "-c:a", "eac3", "-ac", "6", "-b:a", "640000"]
        ));
    }

    #[test]
    fn an_alac_track_is_encoded_by_ffmpeg_s_own_encoder_with_no_rate() {
        let args = args(Recipe::Audio(AudioRecipe {
            stream: 1,
            codec: AudioCodec::Alac,
            channels: 2,
            bitrate: 0,
        }));
        assert!(
            has(&args, &["-c:a", "alac", "-ac", "2"]) && !args.iter().any(|a| a == "-b:a"),
            "{args:?}"
        );
    }

    #[test]
    fn a_track_keeps_the_file_s_times() {
        assert!(args(DTS_AS_EAC3).iter().any(|a| a == "-copyts"));
    }

    #[test]
    fn a_track_is_written_bit_exact_with_no_metadata() {
        assert!(has(
            &args(DTS_AS_EAC3),
            &[NO_METADATA, BITEXACT_ARGS].concat()
        ));
    }

    #[test]
    fn a_video_track_maps_no_audio() {
        let args = args(Recipe::Video(MOBILE));
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("0:1") || a.starts_with("0:2")),
            "{args:?}"
        );
    }

    #[test]
    fn a_tone_mapped_video_track_tone_maps() {
        let args = args(Recipe::Video(CopyVideo {
            tone_map: true,
            ..MOBILE
        }));
        let graph = args
            .iter()
            .skip_while(|a| *a != "-filter_complex")
            .nth(1)
            .unwrap();
        assert!(graph.contains("tonemap="), "{graph}");
    }

    #[test]
    fn a_track_is_matroska_written_to_its_part_file() {
        assert!(args(DTS_AS_EAC3).ends_with(&[
            s("-f"),
            s("matroska"),
            s("/optimized/making/t1.mkv.part")
        ]));
    }

    #[test]
    fn a_track_is_not_written_as_a_live_stream() {
        assert!(!args(Recipe::Video(MOBILE)).iter().any(|a| a == "-live"));
    }
}
