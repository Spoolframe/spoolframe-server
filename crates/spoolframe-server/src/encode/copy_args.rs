//! A copy's ffmpeg command line (PX-37): the video converted as its
//! profile (or the playback it is made ahead for) says, every audio track,
//! every subtitle in the file and its attachments, into one Matroska file
//! with cues.
//!
//! The video's part is a conversion's own ([`input_video_args`]): the same
//! decode, scale and tone map as a single encode of that plan (PGPU-04).
//! What follows maps each stream of the file by its index, the video from
//! the graph being output stream 0, and names each one's codec by its
//! output index. Language, title and dispositions come with each mapped
//! stream: ffmpeg copies them from the input by default.

use spoolframe_media::{AudioStream, SubtitleKind, SubtitleSource, codecs};
use spoolframe_types::AudioCodec;

use super::args::{EncodeSpec, Inputs, Output, PROGRESS_ARGS, input_video_args, s};
use crate::planner::{MAX_ENCODED_CHANNELS, encoded_audio_bitrate};

/// The text subtitle codecs Matroska keeps as they are; any other text
/// is written as SubRip.
const KEPT_TEXT: [&str; 5] = ["subrip", "srt", "ass", "ssa", "webvtt"];

/// Text subtitles that are not text ffmpeg can write again: teletext
/// decodes to pictures or needs a library builds often leave out, and
/// closed captions ride in the video. Left out of a copy, so the copy is
/// made at all.
pub(crate) const UNWRITABLE_TEXT: [&str; 2] = ["dvb_teletext", "eia_608"];

/// ffmpeg's arguments for a copy of `spec.descriptor`'s file, written to
/// `spec.output`'s file (`Output::Progressive`, the copy's `.part`).
/// `spec.video` is the copy's (`CopyVideo` into `ConvertedVideo`); a plan's audio,
/// subtitle and sidecar are not a copy's and are refused. The error says
/// why there are no arguments: no encoder for the video, or none for
/// audio that has to be converted.
pub(crate) fn copy_args(spec: &EncodeSpec<'_>) -> Result<Vec<String>, String> {
    let Output::Progressive { file } = &spec.output else {
        return Err("a copy is one Matroska file".into());
    };
    if spec.audio.is_some()
        || spec.subtitle.is_some()
        || spec.inputs != Inputs::film(spec.inputs.film)
    {
        return Err("a copy keeps every track of its file, not a plan's".into());
    }
    let d = spec.descriptor;
    let mut args = input_video_args(spec)?;
    // Output stream 0 is the video, from the graph.
    let mut out = 1;
    for track in &d.audio {
        args.extend([s("-map"), format!("0:{}", track.index)]);
        args.extend(audio_codec_args(spec, track, out)?);
        out += 1;
    }
    for track in &d.subtitles {
        // A subtitle file beside the film stays there: it is not the
        // file's, and the server serves it as it is.
        let SubtitleSource::Embedded { index } = track.source else {
            continue;
        };
        let codec = match track.kind {
            SubtitleKind::Bitmap => "copy",
            SubtitleKind::Text if KEPT_TEXT.contains(&track.codec.as_str()) => "copy",
            SubtitleKind::Text if UNWRITABLE_TEXT.contains(&track.codec.as_str()) => continue,
            SubtitleKind::Text => "srt",
        };
        args.extend([
            s("-map"),
            format!("0:{index}"),
            format!("-c:{out}"),
            s(codec),
        ]);
        out += 1;
    }
    // Fonts for ASS, and whatever else the file carries.
    args.extend(["-map", "0:t?", "-c:t", "copy"].map(s));
    // As a conversion's output has it: some records the Matroska muxer
    // writes only with it (a Dolby Vision configuration, PGPU-06).
    args.extend(["-strict", "unofficial"].map(s));
    args.extend(PROGRESS_ARGS.map(s));
    // Not `-live`: the muxer goes back to write the cues and sizes, so the
    // copy seeks anywhere at once. Nobody reads it before it is whole.
    args.extend([s("-f"), s("matroska"), file.display().to_string()]);
    Ok(args)
}

/// The codec arguments for audio `track`, output stream `out`: AAC, AC-3
/// and E-AC-3 copied, as a client plays them as they are; any other
/// converted to E-AC-3, or AAC where the server has no E-AC-3 encoder, at
/// a playback's channels and bitrate for it.
fn audio_codec_args(
    spec: &EncodeSpec<'_>,
    track: &AudioStream,
    out: usize,
) -> Result<Vec<String>, String> {
    if matches!(
        codecs::audio_codec(&track.codec),
        AudioCodec::Aac | AudioCodec::Ac3 | AudioCodec::Eac3
    ) {
        return Ok(vec![format!("-c:{out}"), s("copy")]);
    }
    let (codec, encoder) = [AudioCodec::Eac3, AudioCodec::Aac]
        .into_iter()
        .find_map(|codec| Some((codec, spec.encoders.audio_encoder(codec)?)))
        .ok_or_else(|| format!("audio {}: no encoder for eac3 or aac", track.index))?;
    let channels = track.channels.unwrap_or(2).clamp(1, MAX_ENCODED_CHANNELS);
    Ok(vec![
        format!("-c:{out}"),
        encoder.name.clone(),
        format!("-ac:{out}"),
        s(channels),
        format!("-b:{out}"),
        s(encoded_audio_bitrate(codec, channels)),
    ])
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use spoolframe_media::encoders::{AudioEncoder, Encoders, ToneMapSetting, VideoEncoder};
    use spoolframe_media::{MediaDescriptor, SubtitleStream, VideoStream};
    use spoolframe_types::{AudioRole, HdrFormat, SubtitleRole, VideoCodec};

    use super::*;
    use crate::store::optimized::CopyVideo;

    /// A 4K HDR10 HEVC MKV: stream 0 video, 1 DTS 5.1, 2 AC-3 5.1, 3 PGS,
    /// 4 SRT, 5 MP4 timed text; and an SRT file beside it.
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
        let sub = |source, codec: &str| SubtitleStream {
            source,
            kind: SubtitleKind::of_codec(codec),
            codec: codec.into(),
            language: Some("eng".into()),
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
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
            subtitles: vec![
                sub(SubtitleSource::Embedded { index: 3 }, "hdmv_pgs_subtitle"),
                sub(SubtitleSource::Embedded { index: 4 }, "subrip"),
                sub(SubtitleSource::Embedded { index: 5 }, "mov_text"),
                sub(
                    SubtitleSource::External {
                        path: "/media/Film.en.srt".into(),
                    },
                    "subrip",
                ),
            ],
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
    };

    /// The arguments for a copy of `descriptor` as `video` says, with
    /// `encoders`.
    fn args_with(
        descriptor: &MediaDescriptor,
        video: CopyVideo,
        encoders: &Encoders,
    ) -> Vec<String> {
        copy_args(&EncodeSpec {
            inputs: Inputs::film(Path::new("/optimized/making/c1.input")),
            descriptor,
            video: Some(video.into()),
            audio: None,
            subtitle: None,
            encoders,
            vaapi_device: Path::new(""),
            hardware_decode: true,
            gpu_filters: false,
            tone_map: ToneMapSetting::Auto,
            start: None,
            output: Output::Progressive {
                file: "/optimized/making/c1.mkv.part".into(),
            },
        })
        .unwrap()
    }

    fn args(video: CopyVideo) -> Vec<String> {
        args_with(&film(), video, &encoders())
    }

    /// Whether `args` hold `run`, one after the other.
    fn has(args: &[String], run: &[&str]) -> bool {
        args.windows(run.len()).any(|w| w == run)
    }

    #[test]
    fn a_dts_track_is_converted_to_e_ac_3_at_a_playback_s_rate() {
        assert!(has(
            &args(MOBILE),
            &[
                "-map", "0:1", "-c:1", "eac3", "-ac:1", "6", "-b:1", "640000"
            ]
        ));
    }

    #[test]
    fn an_ac_3_track_is_copied() {
        assert!(has(&args(MOBILE), &["-map", "0:2", "-c:2", "copy"]));
    }

    #[test]
    fn without_an_e_ac_3_encoder_a_dts_track_is_converted_to_aac() {
        let aac_only = Encoders::software_baseline();
        assert!(has(
            &args_with(&film(), MOBILE, &aac_only),
            &["-map", "0:1", "-c:1", "aac", "-ac:1", "6", "-b:1", "384000"]
        ));
    }

    #[test]
    fn a_picture_subtitle_is_copied() {
        assert!(has(&args(MOBILE), &["-map", "0:3", "-c:3", "copy"]));
    }

    #[test]
    fn a_subrip_subtitle_is_kept_as_it_is() {
        assert!(has(&args(MOBILE), &["-map", "0:4", "-c:4", "copy"]));
    }

    #[test]
    fn other_text_subtitles_are_kept_as_subrip() {
        assert!(has(&args(MOBILE), &["-map", "0:5", "-c:5", "srt"]));
    }

    #[test]
    fn the_attachments_are_kept() {
        assert!(has(&args(MOBILE), &["-map", "0:t?", "-c:t", "copy"]));
    }

    #[test]
    fn a_tone_mapped_copy_tone_maps_its_video() {
        let sdr = CopyVideo {
            tone_map: true,
            ..MOBILE
        };
        let args = args(sdr);
        let graph = args
            .iter()
            .skip_while(|a| *a != "-filter_complex")
            .nth(1)
            .unwrap();
        assert!(graph.contains("tonemap="), "{graph}");
    }

    #[test]
    fn a_subtitle_file_beside_the_film_is_not_read() {
        let inputs = args(MOBILE).iter().filter(|a| *a == "-i").count();
        assert_eq!(inputs, 1);
    }

    #[test]
    fn a_copy_is_not_written_as_a_live_stream() {
        assert!(!args(MOBILE).iter().any(|a| a == "-live"));
    }

    #[test]
    fn a_copy_is_matroska_written_to_its_part_file() {
        assert!(args(MOBILE).ends_with(&[
            s("-f"),
            s("matroska"),
            s("/optimized/making/c1.mkv.part")
        ]));
    }

    #[test]
    fn a_copy_keeps_the_unofficial_records() {
        assert!(has(&args(MOBILE), &["-strict", "unofficial"]));
    }
}
