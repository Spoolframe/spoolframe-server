//! ffmpeg's whole command line: the inputs and the video, then the
//! audio, subtitles and output (P2-03).

use std::path::{Path, PathBuf};

use spoolframe_media::encoders::{Hardware, cuda_device_args, hwaccel_args};
use spoolframe_media::{SubtitleKind, SubtitleSource, VideoStream};
use spoolframe_protocol::{AudioAction, ConvertedVideo, SubtitleAction};
use spoolframe_types::{SubtitleFormat, VideoCodec};

use super::*;

/// The arguments, or why there are none: a plan naming an encoder this
/// machine does not have, or a stream the file does not have.
pub fn ffmpeg_args(spec: &EncodeSpec<'_>) -> Result<Vec<String>, String> {
    let mut args = input_video_args(spec)?;
    output_args(spec, &mut args)?;
    Ok(args)
}

/// What asks ffmpeg for its progress, on stdout, for the job to read.
pub(crate) const PROGRESS_ARGS: [&str; 3] = ["-progress", "pipe:1", "-nostats"];

/// The first part of the arguments: ffmpeg's own options, the devices and
/// decode, the inputs with their seeks, and the video's map, graph and
/// encoder.
pub(crate) fn input_video_args(spec: &EncodeSpec<'_>) -> Result<Vec<String>, String> {
    let d = spec.descriptor;
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]
        .map(s)
        .to_vec();

    // The video encoder, if any: VA-API wants its device before the inputs.
    let encoder = match spec.video {
        Some(ConvertedVideo::Encode { codec, .. }) => Some(
            placement(spec)
                .map(|place| place.encoder)
                .ok_or_else(|| format!("no encoder for {codec} on this machine"))?,
        ),
        _ => None,
    };
    let vaapi = encoder.is_some_and(|e| e.hardware == Some(Hardware::Vaapi));
    let decode = decode(spec);
    let placebo = libplacebo(spec);
    // An option of the film's input, so before its seek.
    let decoder = gpu_decoder(spec);
    if let Some((family, device)) = placebo {
        args.extend(libplacebo_devices(spec, family, device, decode));
    } else if vaapi && decode != Decode::Gpu {
        // Frames decoded on the GPU and left there bring their device to
        // the encoder; uploaded ones need it named.
        args.extend([s("-vaapi_device"), s(spec.vaapi_device.display())]);
    }
    if placebo.is_some() {
        // Its devices and decode are set.
    } else if decode == Decode::Gpu && matches!(burned(spec), Ok(Some(_))) {
        // The pictures are uploaded to the device the film decodes on:
        // one device, named, for both.
        if decoder == Some(Hardware::Nvenc) {
            args.extend(cuda_device_args());
        } else {
            args.extend([
                s("-init_hw_device"),
                format!("vaapi=gpu:{}", spec.vaapi_device.display()),
                s("-filter_hw_device"),
                s("gpu"),
            ]);
            args.extend(["-hwaccel", "vaapi", "-hwaccel_device", "gpu"].map(s));
            args.extend(["-hwaccel_output_format", "vaapi"].map(s));
        }
    } else if let Some(decoder) = decoder {
        // The frames back in memory, or kept on the GPU.
        args.extend(hwaccel_args(
            decoder,
            spec.vaapi_device,
            decode == Decode::Gpu,
        ));
    }

    let seek = |args: &mut Vec<String>| {
        if let Some(start) = spec.start {
            args.extend([s("-ss"), seconds(start)]);
        }
    };
    for input in ffmpeg_inputs(spec) {
        if input.sought {
            seek(&mut args);
        }
        args.extend([s("-i"), s(input.path.display())]);
    }
    if spec.start.is_some() || input_index(spec, Input::Subtitle).is_some() {
        // The film's own time, not 0, so the player shows where it is
        // and HLS segments line up with the playlist and the cues.
        // And for a subtitle file, burned in or kept as a stream: without
        // it ffmpeg moves a picture file's times back by its first cue's,
        // and every cue shows early.
        args.push(s("-copyts"));
    }

    // Video. A plan with video over a file without any is refused, as an
    // audio track that is not there is: written as audio alone, it would
    // play as nothing anyone asked for.
    if spec.video.is_some() && d.video.is_none() {
        return Err(s("video: no such stream"));
    }
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
                let ten_bits = ten_bits_out(spec, codec, tone_map, encoder.hardware);
                // A size AV1 on VA-API cannot write as it is: the graph
                // keeps the source's, and the frames, on the GPU at its
                // end whichever path they took, are scaled there to one it
                // can ([`whole_av1_size`]).
                let whole = whole_av1_size(spec, stream, height);
                let height = if whole.is_some() {
                    stream.height
                } else {
                    height
                };
                let graph = if let Some(placebo) = placebo {
                    libplacebo_graph(spec, stream, height, placebo, decode)?
                } else if decode == Decode::Gpu && decoder == Some(Hardware::Nvenc) {
                    cuda_graph(spec, stream, height, ten_bits)?
                } else if decode == Decode::Gpu {
                    gpu_graph(spec, stream, height, tone_map, ten_bits)?
                } else {
                    cpu_graph(spec, stream, codec, height, tone_map, vaapi, ten_bits)?
                };
                let graph = match whole {
                    Some((w, h)) => {
                        let graph = graph.strip_suffix("[v]").unwrap_or(&graph);
                        let mut steps = vaapi_steps((stream.width, stream.height), (w, h));
                        steps.push(format!("scale_vaapi=w={w}:h={h}"));
                        format!("{graph},{}[v]", steps.join(","))
                    }
                    None => graph,
                };
                args.extend([s("-filter_complex"), graph, s("-map"), s("[v]")]);
                args.extend([s("-c:v"), encoder.name.clone()]);
                if ten_bits {
                    if codec == VideoCodec::Hevc {
                        args.extend([s("-profile:v"), s("main10")]);
                    }
                    // The HDR kept (PGPU-06): its colours said in the
                    // stream as well as carried on the frames, whatever
                    // the encoder takes from them. Its HDR10 metadata
                    // (mastering display, light level) the encoder
                    // carries from the frames.
                    if let Some(transfer) = kept_transfer(stream) {
                        args.extend([
                            s("-color_primaries"),
                            s("bt2020"),
                            s("-color_trc"),
                            s(transfer),
                            s("-colorspace"),
                            s("bt2020nc"),
                        ]);
                    }
                }
                if let Some(rate) = max_bitrate {
                    args.extend([
                        s("-b:v"),
                        s(rate),
                        s("-maxrate"),
                        s(rate),
                        s("-bufsize"),
                        s(rate.saturating_mul(2)),
                    ]);
                }
                match &spec.output {
                    // A remux ladder's lower rung (PX-21): a keyframe at
                    // each of the copy's cuts and nowhere else, so its
                    // segments are the copy's.
                    Output::Hls {
                        cuts: Cuts::Forced(cuts),
                        ..
                    } => {
                        args.extend([s("-force_key_frames"), keyframes_at(cuts, spec.start)]);
                        args.extend(only_forced_keyframes(&encoder.name));
                    }
                    // A keyframe at every cut and nowhere else, so HLS
                    // cuts land on the grid. The muxer cuts at the first
                    // keyframe past each threshold, and a restart's
                    // thresholds can come early (see
                    // `copied_audio_from_start`): a keyframe of the
                    // encoder's own there, VA-API's every 120 frames,
                    // cut every segment of a resumed film a second off
                    // the playlist's grid.
                    Output::Hls { .. } => {
                        args.extend([s("-force_key_frames"), forced_keyframes(from_start(spec))]);
                        args.extend(only_forced_keyframes(&encoder.name));
                    }
                    Output::Progressive { .. } => {}
                }
            }
            ConvertedVideo::Unknown => return Err(s("a video action this build does not know")),
        }
    }
    Ok(args)
}

/// What one of ffmpeg's inputs is read for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Input {
    Film,
    /// The subtitle file, burned in as pictures or kept as a stream.
    Subtitle,
    /// The optimized copy the audio is taken from (PX-37).
    Audio,
}

/// One `-i`: its file, and whether it is sought as the film is.
struct FfmpegInput {
    what: Input,
    path: PathBuf,
    sought: bool,
}

/// ffmpeg's inputs in their `-i` order: the film, a subtitle file it
/// reads ([`sidecar_input`]), then the copy the audio is taken from
/// (PX-37), sought as the film is. Every input's number comes from this
/// list ([`input_index`]), so a `-map` cannot name another's.
fn ffmpeg_inputs(spec: &EncodeSpec<'_>) -> Vec<FfmpegInput> {
    let mut inputs = vec![FfmpegInput {
        what: Input::Film,
        path: spec.inputs.film.to_path_buf(),
        sought: true,
    }];
    if let Some((path, sought)) = sidecar_input(spec) {
        inputs.push(FfmpegInput {
            what: Input::Subtitle,
            path,
            sought,
        });
    }
    if let Some(copy) = spec.inputs.audio {
        inputs.push(FfmpegInput {
            what: Input::Audio,
            path: copy.to_path_buf(),
            sought: true,
        });
    }
    inputs
}

/// ffmpeg's number for the input read for `what`, if it reads one.
pub(crate) fn input_index(spec: &EncodeSpec<'_>, what: Input) -> Option<usize> {
    ffmpeg_inputs(spec).iter().position(|i| i.what == what)
}

/// The subtitle file ffmpeg reads, and whether it is sought as the film
/// is; `None` for no such input.
fn sidecar_input(spec: &EncodeSpec<'_>) -> Option<(PathBuf, bool)> {
    let sub = spec.subtitle?;
    let track = spec.descriptor.subtitles.get(sub.index as usize)?;
    let SubtitleSource::External { path } = &track.source else {
        return None;
    };
    let path = spec
        .inputs
        .sidecar
        .map_or_else(|| path.clone(), Path::to_path_buf);
    // External pictures to burn in: laid over the video, as embedded ones
    // are. (External text is burned in by a filter that reads the file
    // itself.)
    let burned_pictures =
        matches!(burned(spec), Ok(Some(track)) if track.kind == SubtitleKind::Bitmap);
    if burned_pictures {
        // Read from its start: a cue that began before the seek is still
        // drawn, and ffmpeg waits for ever on a picture file sought past
        // its last cue.
        Some((path, false))
    } else if !matches!(
        sub.action,
        SubtitleAction::BurnIn { .. } | SubtitleAction::Ocr { .. }
    ) {
        // Kept as a stream, sought the same way.
        Some((path, true))
    } else {
        None
    }
}

/// The input the audio is read from: an optimized copy (PX-37), else
/// the film, always ffmpeg's first.
fn audio_input(spec: &EncodeSpec<'_>) -> usize {
    input_index(spec, Input::Audio).unwrap_or(0)
}

/// The rest, after [`input_video_args`]: the audio, a subtitle kept as a
/// stream, the progress, and the output's muxer and files.
fn output_args(spec: &EncodeSpec<'_>, args: &mut Vec<String>) -> Result<(), String> {
    let d = spec.descriptor;

    // Audio.
    if let Some(audio) = spec.audio {
        let stream = d
            .audio
            .get(audio.index as usize)
            .ok_or_else(|| format!("audio {}: no such track", audio.index))?;
        args.extend([s("-map"), format!("{}:{}", audio_input(spec), stream.index)]);
        match audio.action {
            AudioAction::Copy { .. } => {
                args.extend([s("-c:a"), s("copy")]);
                args.extend(copied_audio_from_start(spec));
            }
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
                // The MP4 muxer writes an AC-3 or E-AC-3 sample entry from
                // the track's first packet, and the HLS muxer writes
                // `init.mp4` at its first cut: a track that starts after
                // that cut gets a `moov` of size 0, and nothing plays.
                // Silence from the run's start puts a packet first.
                // `first_pts` counts samples at the rate set here; the run's
                // times are the film's own from a seek (`-copyts`).
                if let Output::Hls {
                    pad_audio: true, ..
                } = spec.output
                {
                    let from = spec.start.map_or(0, |at| at.as_millis()) * 48;
                    args.extend([
                        s("-af"),
                        format!("aresample=48000:async=1:first_pts={from}"),
                    ]);
                }
            }
            AudioAction::Unknown => return Err(s("an audio action this build does not know")),
        }
    }

    // A subtitle kept as a stream. HLS carries text as a WebVTT rendition
    // of its own (P2-07), not in the segments.
    if let Some(sub) = spec.subtitle
        && matches!(spec.output, Output::Progressive { .. })
    {
        let track = d.subtitles.get(sub.index as usize);
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
        if let (Some(track), Some(codec)) = (track, codec) {
            let from = pictures_input(spec, &track.source)?;
            args.extend([s("-map"), from, s("-c:s"), s(codec)]);
        }
    }

    // A Dolby Vision copy the client shows as such (PGPU-06): the muxers
    // write its configuration record (MP4's `dvcC`, or `dvvC` from profile
    // 8; Matroska's block addition mapping) only with `-strict
    // unofficial`. Without it a Dolby Vision stream copied is its base
    // layer to the player. Checked on profile 8.1 into HLS fMP4: `dvvC`
    // with it, none without.
    if matches!(
        spec.video,
        Some(ConvertedVideo::Copy {
            dolby_vision: true,
            ..
        })
    ) {
        args.extend([s("-strict"), s("unofficial")]);
    }
    args.extend(PROGRESS_ARGS.map(s));
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
        Output::Hls {
            dir,
            start_number,
            cuts,
            ..
        } => {
            // A copied video is cut at every keyframe (see `hls`): a length
            // under any GOP does that. So is a remux ladder's encoded rung,
            // whose keyframes are the copy's cuts and no others.
            let cut = match spec.video {
                Some(ConvertedVideo::Copy { .. }) => s("0.1"),
                _ if matches!(cuts, Cuts::Forced(_)) => s("0.1"),
                _ => s(SEGMENT_SECS),
            };
            // An encoded video's run from 0 cuts its short head (PX-26):
            // the muxer's thresholds are `hls_init_time` × n until its list
            // is full, then go on by `hls_time` from the list's end. With
            // one entry that is 2 s, 4 s, 8 s, then every 6 s: the grid of
            // `hls::Segments::encoded`. The list is ffmpeg's own playlist,
            // which the server reads only for its last segment (`job`).
            let list_size = match spec.video {
                Some(ConvertedVideo::Encode { .. }) if from_start(spec) && *cuts == Cuts::Own => {
                    args.extend([s("-hls_init_time"), s(FIRST_SEGMENT_SECS)]);
                    s(1)
                }
                _ => s(0),
            };
            // ffmpeg names HEVC in MP4 `hev1`, copied or from any encoder;
            // Apple's players take only `hvc1`, which is also what the
            // playlist says (`hls`). The parameter sets are in `init.mp4`
            // either way.
            if matches!(
                spec.video,
                Some(
                    ConvertedVideo::Copy {
                        codec: VideoCodec::Hevc,
                        ..
                    } | ConvertedVideo::Encode {
                        codec: VideoCodec::Hevc,
                        ..
                    }
                )
            ) {
                // Dolby Vision profile 5 labelled: `dvh1`, which only a
                // player that shows Dolby Vision plays; profile 8 keeps
                // `hvc1`, its base layer playable as HDR10 or HLG
                // (PGPU-06).
                let p5 = d.video.as_ref().is_some_and(VideoStream::needs_rpu);
                let tag = match spec.video {
                    Some(ConvertedVideo::Copy {
                        dolby_vision: true, ..
                    }) if p5 => "dvh1",
                    _ => "hvc1",
                };
                args.extend([s("-tag:v"), s(tag)]);
            }
            // `hvc1` also means the parameter sets are in the sample
            // entry's `hvcC`, and AVPlayer decodes with those alone. A
            // copied stream's `hvcC` is the source's record as it is, and
            // some Matroska files (4K WEB-DLs) keep the parameter sets only
            // in the stream, before each keyframe: the record has none, and
            // AVPlayer refuses the stream. As Annex B the MP4 muxer reads
            // them from the first keyframe into a whole record, whatever
            // the source's was, and stores the samples length-prefixed
            // again.
            if matches!(
                spec.video,
                Some(ConvertedVideo::Copy {
                    codec: VideoCodec::Hevc,
                    ..
                })
            ) {
                // Dolby Vision profile 7's enhancement layer (NAL units of
                // type 63) is a UHD Blu-ray's second stream, which no
                // streaming player decodes: left out, the base layer and
                // its RPU go on as profile 8's do.
                let p7 = d
                    .video
                    .as_ref()
                    .and_then(|v| v.dolby_vision)
                    .is_some_and(|dv| dv.profile == 7);
                let bsf = if p7 {
                    "hevc_mp4toannexb,filter_units=remove_types=63"
                } else {
                    "hevc_mp4toannexb"
                };
                args.extend([s("-bsf:v"), s(bsf)]);
            }
            if let Cuts::Listed { starts, from_ms } = cuts {
                let Some(ConvertedVideo::Copy { dolby_vision, .. }) = spec.video else {
                    return Err(s("a segment list is for a copied video only"));
                };
                let times = match segment_times(starts, *start_number, *from_ms).0 {
                    list if list.is_empty() => s(NO_CUT),
                    list => list,
                };
                // The fMP4 the HLS muxer writes: fragments only where the
                // segment muxer flushes them, at its cuts (`frag_custom`),
                // with a `sidx` each (`dash`), the film's own times
                // (`frag_discont`, as below; without `delay_moov` a run
                // from a seek stores them from 0 all the same). The
                // segment muxer does not hand its own `-strict` to the MP4
                // muxer.
                let mut options = s("movflags=+frag_custom+dash+delay_moov+frag_discont");
                if dolby_vision {
                    options.push_str(":strict=unofficial");
                }
                // A run from a seek starts at the keyframe at or before
                // where it is aimed, a millisecond before its cut
                // (`segment_seek`), and on a source keyed more often than
                // it is cut, or with B-frames, before the cut: the copied
                // streams drop what is before the seek, so the run's first
                // video packet is its cut's keyframe, which the times
                // above are measured from.
                if spec.start.is_some_and(|start| !start.is_zero()) {
                    args.extend([s("-copypriorss"), s("0")]);
                }
                args.extend([
                    s("-f"),
                    s("segment"),
                    s("-segment_format"),
                    s("mp4"),
                    s("-segment_format_options"),
                    options,
                    // The header once, before the first packet, from the
                    // stream's record: whole only where the record holds
                    // the parameter sets, and not for AC-3 or E-AC-3,
                    // whose MP4 entry needs packets first (`encode`
                    // picks the HLS muxer for those).
                    s("-segment_header_filename"),
                    s(dir.join("init.mp4").display()),
                    s("-individual_header_trailer"),
                    s("0"),
                    // The keyframe index rounds to the millisecond: a
                    // cut's keyframe may be up to half of one early.
                    s("-segment_time_delta"),
                    s("0.001"),
                    s("-segment_times"),
                    times,
                    s("-segment_start_number"),
                    s(start_number),
                    // Rewritten aside and renamed after each segment is
                    // closed, as the HLS muxer's playlist is: the server
                    // reads it to know which segments are finished.
                    s("-segment_list"),
                    s(dir.join(PLAYLIST).display()),
                    s("-segment_list_type"),
                    s("m3u8"),
                    s(dir.join("seg%d.m4s").display()),
                ]);
                return Ok(());
            }
            args.extend([
                s("-f"),
                s("hls"),
                s("-hls_time"),
                cut,
                s("-hls_segment_type"),
                s("fmp4"),
                s("-hls_fmp4_init_filename"),
                s("init.mp4"),
                // The segments store the times ffmpeg gives them: after a
                // seek, the film's own (`-copyts`). Without this the MP4
                // muxer stores every run from 0 and puts where it started
                // in `init.mp4` as an empty edit, which AVPlayer does not
                // apply when it places WebVTT cues (P2-07).
                s("-hls_segment_options"),
                s("movflags=+frag_discont"),
                s("-hls_segment_filename"),
                s(dir.join("seg%d.m4s").display()),
                s("-start_number"),
                s(start_number),
                s("-hls_list_size"),
                list_size,
                // ffmpeg's own playlist; the server writes the one clients
                // see (P2-05), and reads this one to know which segments
                // are finished.
                s(dir.join(PLAYLIST).display()),
            ]);
        }
    }
    Ok(())
}
