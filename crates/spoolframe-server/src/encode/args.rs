//! ffmpeg's arguments for a converted plan (P2-03).
//!
//! Filters run on the CPU and the frames go to the encoder after them, so
//! scaling, tone mapping and burn-in work with any encoder; a VA-API
//! encoder gets its frames uploaded at the end of the chain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_media::encoders::{Encoders, Hardware};
use spoolframe_media::{MediaDescriptor, SubtitleKind, SubtitleSource};
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, SubtitleAction,
};
use spoolframe_types::{SubtitleFormat, VideoCodec};

/// HLS segment length (P2-05): Apple's recommendation.
pub const SEGMENT_SECS: u32 = 6;

/// ffmpeg's own HLS playlist, in the job's directory.
pub const PLAYLIST: &str = "ffmpeg.m3u8";

/// Where and how the job writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// One Matroska file, readable while it grows (P2-04).
    Progressive { file: PathBuf },
    /// fMP4 segments `seg<n>.m4s` and `init.mp4` in `dir`, numbered from
    /// `start_number` (P2-05).
    Hls { dir: PathBuf, start_number: u32 },
}

/// One encode: the plan's parts and what they run on.
#[derive(Debug, Clone)]
pub struct EncodeSpec<'a> {
    /// A plain path (see `filter_path`): the job's link to the file.
    pub input: &'a Path,
    /// The job's link to the selected external subtitle, if it is one;
    /// the descriptor's own path is not used, for the same reason.
    pub sidecar: Option<&'a Path>,
    pub descriptor: &'a MediaDescriptor,
    pub video: Option<ConvertedVideo>,
    pub audio: Option<ConvertedAudio>,
    pub subtitle: Option<ConvertedSubtitle>,
    pub encoders: &'a Encoders,
    pub vaapi_device: &'a Path,
    pub start: Option<Duration>,
    pub output: Output,
}

/// HDR to SDR (BT.709) with the Hable curve, in software: it works for any
/// source and any encoder, where `tonemap_vaapi` needs a VA-API decode.
const TONE_MAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
                        tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,\
                        format=yuv420p";

fn s(v: impl ToString) -> String {
    v.to_string()
}

/// A path for a filter argument (`subtitles=filename=…`). Filter graphs
/// have two layers of escaping; rather than get them right for every name
/// in a library, the job links its inputs into its own directory under
/// plain names, and a path that would need escaping is refused.
fn filter_path(path: &Path) -> Result<String, String> {
    let raw = path.display().to_string();
    if raw.contains(['\'', ':', '\\', ',', ';', '[', ']']) {
        return Err(format!("{raw}: not a plain path for a filter"));
    }
    Ok(format!("'{raw}'"))
}

fn seconds(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// The arguments, or why there are none: a plan naming an encoder this
/// machine does not have, or a stream the file does not have.
pub fn ffmpeg_args(spec: &EncodeSpec<'_>) -> Result<Vec<String>, String> {
    let d = spec.descriptor;
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]
        .map(s)
        .to_vec();

    // The video encoder, if any: VA-API wants its device before the inputs.
    let encoder = match spec.video {
        Some(ConvertedVideo::Encode { codec, .. }) => Some(
            spec.encoders
                .video_encoder(codec)
                .ok_or_else(|| format!("no encoder for {codec} on this machine"))?,
        ),
        _ => None,
    };
    let vaapi = encoder.is_some_and(|e| e.hardware == Some(Hardware::Vaapi));
    if vaapi {
        args.extend([s("-vaapi_device"), s(spec.vaapi_device.display())]);
    }

    let seek = |args: &mut Vec<String>| {
        if let Some(start) = spec.start {
            args.extend([s("-ss"), seconds(start)]);
        }
    };
    seek(&mut args);
    args.extend([s("-i"), s(spec.input.display())]);
    // An external subtitle is a second input, sought the same way.
    let sidecar =
        spec.subtitle
            .and_then(|sub| match &d.subtitles.get(sub.index as usize)?.source {
                SubtitleSource::External { path } => {
                    Some(spec.sidecar.map_or_else(|| path.clone(), Path::to_path_buf))
                }
                SubtitleSource::Embedded { .. } => None,
            });
    if let (Some(path), Some(sub)) = (&sidecar, spec.subtitle)
        && !matches!(
            sub.action,
            SubtitleAction::BurnIn { .. } | SubtitleAction::Ocr { .. }
        )
    {
        seek(&mut args);
        args.extend([s("-i"), s(path.display())]);
    }
    if spec.start.is_some() {
        // The film's own time, not 0, so the player shows where it is.
        args.push(s("-copyts"));
    }

    // Video.
    if let (Some(video), Some(stream)) = (spec.video, &d.video) {
        match video {
            ConvertedVideo::Copy { .. } => {
                args.extend([
                    s("-map"),
                    format!("0:{}", stream.index),
                    s("-c:v"),
                    s("copy"),
                ]);
            }
            ConvertedVideo::Encode {
                codec,
                height,
                max_bitrate,
                tone_map,
            } => {
                let encoder = encoder.expect("set above for an encode");
                let mut chain = Vec::new();
                let mut source = format!("[0:{}]", stream.index);
                if let Some(sub) = spec.subtitle
                    && let SubtitleAction::BurnIn { .. } = sub.action
                {
                    let picked = d
                        .subtitles
                        .get(sub.index as usize)
                        .ok_or_else(|| format!("subtitle {}: no such track", sub.index))?;
                    match (&picked.source, picked.kind) {
                        (SubtitleSource::Embedded { index }, SubtitleKind::Bitmap) => {
                            // Pictures go over the picture, their canvas
                            // scaled to it first: Blu-ray PGS is 1080p even
                            // over 4K video, and unscaled it would land
                            // small in the top left.
                            source = format!(
                                "[0:{index}]scale={}:{}[subs];[0:{}][subs]overlay",
                                stream.width, stream.height, stream.index
                            );
                        }
                        (SubtitleSource::Embedded { .. }, SubtitleKind::Text) => {
                            let nth = d.subtitles[..sub.index as usize]
                                .iter()
                                .filter(|s| matches!(s.source, SubtitleSource::Embedded { .. }))
                                .count();
                            chain.push(format!(
                                "subtitles=filename={}:si={nth}",
                                filter_path(spec.input)?
                            ));
                        }
                        (SubtitleSource::External { path }, _) => {
                            let path = spec.sidecar.unwrap_or(path);
                            chain.push(format!("subtitles=filename={}", filter_path(path)?));
                        }
                    }
                }
                if tone_map {
                    chain.push(s(TONE_MAP));
                }
                if height < stream.height {
                    chain.push(format!("scale=-2:{height}"));
                }
                if vaapi {
                    chain.push(s("format=nv12,hwupload"));
                } else if codec == VideoCodec::H264 && !tone_map {
                    // H.264 decoders expect 8-bit 4:2:0.
                    chain.push(s("format=yuv420p"));
                }
                let graph = if chain.is_empty() {
                    format!("{source}null[v]")
                } else if source.ends_with("overlay") {
                    format!("{source},{}[v]", chain.join(","))
                } else {
                    format!("{source}{}[v]", chain.join(","))
                };
                args.extend([s("-filter_complex"), graph, s("-map"), s("[v]")]);
                args.extend([s("-c:v"), encoder.name.clone()]);
                if let Some(rate) = max_bitrate {
                    args.extend([
                        s("-b:v"),
                        s(rate),
                        s("-maxrate"),
                        s(rate),
                        s("-bufsize"),
                        s(rate * 2),
                    ]);
                }
                if matches!(spec.output, Output::Hls { .. }) {
                    // A keyframe every segment, so HLS cuts land on the grid.
                    args.extend([
                        s("-force_key_frames"),
                        format!(
                            "expr:if(isnan(prev_forced_t),1,gte(t,prev_forced_t+{SEGMENT_SECS}))"
                        ),
                    ]);
                }
            }
            ConvertedVideo::Unknown => return Err(s("a video action this build does not know")),
        }
    }

    // Audio.
    if let Some(audio) = spec.audio {
        let stream = d
            .audio
            .get(audio.index as usize)
            .ok_or_else(|| format!("audio {}: no such track", audio.index))?;
        args.extend([s("-map"), format!("0:{}", stream.index)]);
        match audio.action {
            AudioAction::Copy { .. } => args.extend([s("-c:a"), s("copy")]),
            AudioAction::Encode {
                codec,
                channels,
                bitrate,
            } => {
                let encoder = spec
                    .encoders
                    .audio_encoder(codec)
                    .ok_or_else(|| format!("no encoder for {codec} on this machine"))?;
                args.extend([
                    s("-c:a"),
                    encoder.name.clone(),
                    s("-ac"),
                    s(channels),
                    s("-b:a"),
                    s(bitrate),
                ]);
            }
            AudioAction::Unknown => return Err(s("an audio action this build does not know")),
        }
    }

    // A subtitle kept as a stream. HLS carries text as a WebVTT rendition
    // of its own (P2-07), not in the segments.
    if let Some(sub) = spec.subtitle
        && matches!(spec.output, Output::Progressive { .. })
    {
        let from = match &d.subtitles.get(sub.index as usize).map(|t| &t.source) {
            Some(SubtitleSource::Embedded { index }) => Some(format!("0:{index}")),
            Some(SubtitleSource::External { .. }) => Some(s("1:0")),
            None => None,
        };
        let codec = match sub.action {
            SubtitleAction::Copy { .. } => Some("copy"),
            SubtitleAction::Convert { to, .. } => Some(match to {
                SubtitleFormat::WebVtt => "webvtt",
                SubtitleFormat::Ass => "ass",
                SubtitleFormat::MovText => "mov_text",
                _ => "srt",
            }),
            // Read as text by the server and fetched beside the stream.
            SubtitleAction::BurnIn { .. }
            | SubtitleAction::Ocr { .. }
            | SubtitleAction::Unknown => None,
        };
        if let (Some(from), Some(codec)) = (from, codec) {
            args.extend([s("-map"), from, s("-c:s"), s(codec)]);
        }
    }

    args.extend(["-progress", "pipe:1", "-nostats"].map(s));
    match &spec.output {
        Output::Progressive { file } => {
            args.extend([
                s("-f"),
                s("matroska"),
                // Written front to back, with no seek back to fill in sizes,
                // so a reader can follow the file as it grows.
                s("-live"),
                s("1"),
                s("-cluster_time_limit"),
                s("2000"),
                s(file.display()),
            ]);
        }
        Output::Hls { dir, start_number } => {
            // A copied video is cut at every keyframe (see `hls`): a length
            // under any GOP does that.
            let cut = match spec.video {
                Some(ConvertedVideo::Copy { .. }) => s("0.1"),
                _ => s(SEGMENT_SECS),
            };
            args.extend([
                s("-f"),
                s("hls"),
                s("-hls_time"),
                cut,
                s("-hls_segment_type"),
                s("fmp4"),
                s("-hls_fmp4_init_filename"),
                s("init.mp4"),
                s("-hls_segment_filename"),
                s(dir.join("seg%d.m4s").display()),
                s("-start_number"),
                s(start_number),
                s("-hls_list_size"),
                s("0"),
                // ffmpeg's own playlist; the server writes the one clients
                // see (P2-05), and reads this one to know which segments
                // are finished.
                s(dir.join(PLAYLIST).display()),
            ]);
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::encoders::{AudioEncoder, VideoEncoder};
    use spoolframe_media::{AudioStream, SubtitleStream, VideoStream};
    use spoolframe_types::{AudioCodec, AudioRole, HdrFormat, SubtitleRole};

    use super::*;

    /// A 4K HDR10 HEVC MKV: stream 0 video, 1 DTS 5.1, 2 PGS, 3 SRT, and an
    /// external SRT.
    fn film() -> MediaDescriptor {
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
                width: 3840,
                height: 2160,
                pixel_format: Some("yuv420p10le".into()),
                color_transfer: Some("smpte2084".into()),
                frame_rate_millis: Some(23_976),
                bit_depth: 10,
                hdr: Some(HdrFormat::Hdr10),
            }),
            audio: vec![AudioStream {
                index: 1,
                codec: "dts".into(),
                profile: None,
                channels: Some(6),
                channel_layout: None,
                language: Some("eng".into()),
                title: None,
                default: true,
                role: AudioRole::Main,
            }],
            subtitles: vec![
                sub(SubtitleSource::Embedded { index: 2 }, "hdmv_pgs_subtitle"),
                sub(SubtitleSource::Embedded { index: 3 }, "subrip"),
                sub(
                    SubtitleSource::External {
                        path: "/media/Film.en.srt".into(),
                    },
                    "subrip",
                ),
            ],
        }
    }

    fn encoders(vaapi: bool) -> Encoders {
        let hardware = vaapi.then_some(Hardware::Vaapi);
        Encoders {
            video: vec![
                VideoEncoder {
                    codec: VideoCodec::H264,
                    name: if vaapi { "h264_vaapi" } else { "libx264" }.into(),
                    hardware,
                },
                VideoEncoder {
                    codec: VideoCodec::Hevc,
                    name: if vaapi { "hevc_vaapi" } else { "libx265" }.into(),
                    hardware,
                },
            ],
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
            hardware_decoders: Vec::new(),
            unusable: Vec::new(),
        }
    }

    fn spec<'a>(
        film: &'a MediaDescriptor,
        encoders: &'a Encoders,
        video: ConvertedVideo,
        audio: AudioAction,
        subtitle: Option<ConvertedSubtitle>,
        output: Output,
    ) -> EncodeSpec<'a> {
        EncodeSpec {
            input: Path::new("/media/Film.mkv"),
            sidecar: None,
            descriptor: film,
            video: Some(video),
            audio: Some(ConvertedAudio {
                index: 0,
                action: audio,
            }),
            subtitle,
            encoders,
            vaapi_device: Path::new("/dev/dri/renderD128"),
            start: None,
            output,
        }
    }

    fn progressive() -> Output {
        Output::Progressive {
            file: "/scratch/h/0/out.mkv".into(),
        }
    }

    /// `args` contains `slice`, in order and adjacent.
    fn has(args: &[String], slice: &[&str]) -> bool {
        args.windows(slice.len())
            .any(|w| w.iter().zip(slice).all(|(a, b)| a == b))
    }

    fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let i = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag}"));
        &args[i + 1]
    }

    const COPY_HEVC: ConvertedVideo = ConvertedVideo::Copy {
        codec: VideoCodec::Hevc,
    };
    const COPY_DTS: AudioAction = AudioAction::Copy {
        codec: AudioCodec::Dts,
    };

    #[test]
    fn a_remux_copies_both_streams_into_live_matroska() {
        let (film, enc) = (film(), encoders(false));
        let args =
            ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, progressive())).unwrap();
        assert!(has(&args, &["-i", "/media/Film.mkv"]));
        assert!(has(&args, &["-map", "0:0", "-c:v", "copy"]));
        assert!(has(&args, &["-map", "0:1", "-c:a", "copy"]));
        assert!(has(&args, &["-f", "matroska", "-live", "1"]));
        assert_eq!(args.last().unwrap(), "/scratch/h/0/out.mkv");
        assert!(has(&args, &["-progress", "pipe:1"]));
        assert!(!args.contains(&s("-copyts")));
    }

    #[test]
    fn avplayer_hls_copies_hevc_and_encodes_e_ac_3_into_fmp4_segments() {
        let (film, enc) = (film(), encoders(false));
        let output = Output::Hls {
            dir: "/scratch/h/0".into(),
            start_number: 1,
        };
        let eac3 = AudioAction::Encode {
            codec: AudioCodec::Eac3,
            channels: 6,
            bitrate: 640_000,
        };
        let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, eac3, None, output)).unwrap();
        assert!(has(&args, &["-c:a", "eac3", "-ac", "6", "-b:a", "640000"]));
        assert!(has(
            &args,
            &["-f", "hls", "-hls_time", "0.1", "-hls_segment_type", "fmp4"]
        ));
        assert_eq!(
            after(&args, "-hls_segment_filename"),
            "/scratch/h/0/seg%d.m4s"
        );
        assert_eq!(after(&args, "-start_number"), "1");
        // Copied video keeps the source's keyframes.
        assert!(!args.contains(&s("-force_key_frames")));
    }

    #[test]
    fn a_vaapi_encode_scales_tone_maps_and_uploads_last() {
        let (film, enc) = (film(), encoders(true));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: Some(8_000_000),
            tone_map: true,
        };
        let args = ffmpeg_args(&spec(&film, &enc, video, COPY_DTS, None, progressive())).unwrap();
        assert_eq!(after(&args, "-vaapi_device"), "/dev/dri/renderD128");
        // The device comes before the input.
        let device = args.iter().position(|a| a == "-vaapi_device").unwrap();
        let input = args.iter().position(|a| a == "-i").unwrap();
        assert!(device < input);
        let graph = after(&args, "-filter_complex");
        assert!(graph.starts_with("[0:0]zscale=t=linear"), "{graph}");
        assert!(graph.contains("tonemap=tonemap=hable"), "{graph}");
        assert!(graph.contains("scale=-2:1080"), "{graph}");
        assert!(graph.ends_with("format=nv12,hwupload[v]"), "{graph}");
        assert!(has(&args, &["-map", "[v]", "-c:v", "h264_vaapi"]));
        assert!(has(
            &args,
            &[
                "-b:v", "8000000", "-maxrate", "8000000", "-bufsize", "16000000"
            ]
        ));
    }

    #[test]
    fn a_pgs_burn_in_overlays_the_pictures() {
        let (film, enc) = (film(), encoders(false));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 2160,
            max_bitrate: None,
            tone_map: false,
        };
        let burn = ConvertedSubtitle {
            index: 0,
            action: SubtitleAction::BurnIn {
                format: SubtitleFormat::Pgs,
            },
        };
        let args = ffmpeg_args(&spec(
            &film,
            &enc,
            video,
            COPY_DTS,
            Some(burn),
            progressive(),
        ))
        .unwrap();
        let graph = after(&args, "-filter_complex");
        assert_eq!(
            graph,
            "[0:2]scale=3840:2160[subs];[0:0][subs]overlay,format=yuv420p[v]"
        );
        // Burned in: not also a subtitle stream.
        assert!(!args.contains(&s("-c:s")));
    }

    #[test]
    fn a_text_burn_in_renders_the_embedded_track_by_its_subtitle_number() {
        let (film, enc) = (film(), encoders(false));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 2160,
            max_bitrate: None,
            tone_map: false,
        };
        let burn = ConvertedSubtitle {
            index: 1,
            action: SubtitleAction::BurnIn {
                format: SubtitleFormat::Srt,
            },
        };
        let args = ffmpeg_args(&spec(
            &film,
            &enc,
            video,
            COPY_DTS,
            Some(burn),
            progressive(),
        ))
        .unwrap();
        let graph = after(&args, "-filter_complex");
        assert_eq!(graph, "[0:0]subtitles=filename='/media/Film.mkv':si=1[v]");
    }

    #[test]
    fn an_external_srt_is_a_second_input_converted_to_webvtt() {
        let (film, enc) = (film(), encoders(false));
        let convert = ConvertedSubtitle {
            index: 2,
            action: SubtitleAction::Convert {
                from: SubtitleFormat::Srt,
                to: SubtitleFormat::WebVtt,
            },
        };
        let args = ffmpeg_args(&spec(
            &film,
            &enc,
            COPY_HEVC,
            COPY_DTS,
            Some(convert),
            progressive(),
        ))
        .unwrap();
        assert!(has(&args, &["-i", "/media/Film.en.srt"]));
        assert!(has(&args, &["-map", "1:0", "-c:s", "webvtt"]));
    }

    #[test]
    fn a_start_seeks_the_input_and_keeps_the_film_s_time() {
        let (film, enc) = (film(), encoders(false));
        let mut spec = spec(&film, &enc, COPY_HEVC, COPY_DTS, None, progressive());
        spec.start = Some(Duration::from_secs(1800));
        let args = ffmpeg_args(&spec).unwrap();
        assert!(has(&args, &["-ss", "1800.000", "-i", "/media/Film.mkv"]));
        assert!(args.contains(&s("-copyts")));
    }

    #[test]
    fn an_encoded_hls_video_forces_a_keyframe_every_segment() {
        let (film, enc) = (film(), encoders(false));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: None,
            tone_map: true,
        };
        let output = Output::Hls {
            dir: "/scratch/h/0".into(),
            start_number: 4,
        };
        let args = ffmpeg_args(&spec(&film, &enc, video, COPY_DTS, None, output)).unwrap();
        assert_eq!(
            after(&args, "-force_key_frames"),
            "expr:if(isnan(prev_forced_t),1,gte(t,prev_forced_t+6))"
        );
        assert_eq!(after(&args, "-hls_time"), "6");
    }

    #[test]
    fn an_encoder_this_machine_lacks_is_an_error() {
        let (film, enc) = (film(), encoders(false));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::Av1,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        };
        assert!(
            ffmpeg_args(&spec(&film, &enc, video, COPY_DTS, None, progressive()))
                .unwrap_err()
                .contains("av1")
        );
    }

    #[test]
    fn a_path_a_filter_would_misread_is_refused() {
        assert_eq!(
            filter_path(Path::new("/scratch/h/0/input")).unwrap(),
            "'/scratch/h/0/input'"
        );
        assert!(filter_path(Path::new("/m/It's: A Film.srt")).is_err());
    }

    #[test]
    fn the_job_s_link_to_a_sidecar_is_what_ffmpeg_reads() {
        let (film, enc) = (film(), encoders(false));
        let convert = ConvertedSubtitle {
            index: 2,
            action: SubtitleAction::Convert {
                from: SubtitleFormat::Srt,
                to: SubtitleFormat::WebVtt,
            },
        };
        let mut spec = spec(
            &film,
            &enc,
            COPY_HEVC,
            COPY_DTS,
            Some(convert),
            progressive(),
        );
        spec.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
        assert!(has(
            &ffmpeg_args(&spec).unwrap(),
            &["-i", "/scratch/h/0/sidecar"]
        ));
    }

    #[test]
    fn a_subtitle_read_as_text_is_not_in_the_output() {
        let (film, enc) = (film(), encoders(false));
        let ocr = ConvertedSubtitle {
            index: 0,
            action: SubtitleAction::Ocr {
                from: SubtitleFormat::Pgs,
                to: SubtitleFormat::Srt,
            },
        };
        let args = ffmpeg_args(&spec(
            &film,
            &enc,
            COPY_HEVC,
            COPY_DTS,
            Some(ocr),
            progressive(),
        ))
        .unwrap();
        assert!(!args.iter().any(|a| a == "-c:s" || a == "0:2"), "{args:?}");
    }
}
