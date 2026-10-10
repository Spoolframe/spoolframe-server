//! ffmpeg's arguments for a converted plan (P2-03).
//!
//! Filters run on the CPU and the frames go to the encoder after them, so
//! scaling, tone mapping and burn-in work with any encoder; a VA-API
//! encoder gets its frames uploaded at the end of the chain. The video is
//! decoded on the GPU where it can be ([`decode`]), the frames coming
//! back to memory for those filters, or staying on the GPU for a VA-API
//! encode that needs none.

use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_media::encoders::{Encoders, Hardware, TONEMAP_VAAPI, VideoEncoder, hwaccel_args};
use spoolframe_media::{
    MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, VideoStream, codecs,
};
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, SubtitleAction,
};
use spoolframe_types::{SubtitleFormat, VideoCodec};

/// HLS segment length (P2-05): Apple's recommendation.
pub const SEGMENT_SECS: u32 = 6;

/// An encoded video's first segment (PX-26), and its second: what the
/// player waits for before the first frame. See `hls::Segments::encoded`.
pub const FIRST_SEGMENT_SECS: u32 = 2;

/// ffmpeg's own HLS playlist, in the job's directory.
pub const PLAYLIST: &str = "ffmpeg.m3u8";

/// Whether `spec`'s run starts at the film's start: a run of HLS
/// segment 1, which cuts the encoded grid's short head.
fn from_start(spec: &EncodeSpec<'_>) -> bool {
    spec.start.is_none_or(|start| start.is_zero())
}

/// `-force_key_frames` for an HLS encode: a keyframe at each cut of the
/// grid, from the run's first frame on. ffmpeg's `t` is the time since
/// that frame, the same origin the HLS muxer measures its cuts from, so
/// each target is where the muxer cuts and the keyframes do not drift off
/// the grid (`prev_forced_t + 6` lands a frame late each time: at
/// 23.976 fps, 6 ms a segment, 7 s over two hours). A run from 0 cuts at
/// 0, 2, 4, 8, 14… s (PX-26); any other at 0, 6, 12… s from its start.
fn forced_keyframes(from_start: bool) -> String {
    let (first, step) = (FIRST_SEGMENT_SECS, SEGMENT_SECS);
    if from_start {
        // The n-th forced keyframe (from 0) at `first × n` for n < 3,
        // then at `first + step × (n - 2)`.
        format!("expr:gte(t,if(lt(n_forced,3),{first}*n_forced,{first}+{step}*(n_forced-2)))")
    } else {
        format!("expr:gte(t,{step}*n_forced)")
    }
}

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
    /// The video may be decoded on the GPU: false once that failed for
    /// this file (see [`decode`]).
    pub hardware_decode: bool,
    /// Tone map and burn in pictures on the GPU where the driver can
    /// (`transcode.gpu_filters`).
    pub gpu_filters: bool,
    pub start: Option<Duration>,
    pub output: Output,
}

/// Where the video is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decode {
    /// In software.
    Software,
    /// On the GPU, with the frames back in memory for the filters on the
    /// CPU.
    Hardware,
    /// On the GPU, and the frames stay there: scaled on it and encoded
    /// from it, for a video that needs no filter on the CPU.
    Gpu,
}

/// Where `spec`'s video is decoded: on the GPU when detection found a
/// VA-API decoder for its codec and it is encoded with VA-API or in
/// software, unless that failed for this file before; and with the
/// frames left on the GPU when a VA-API encoder takes them there and no
/// filter on the CPU needs them.
///
/// A software encode gains too. Measured for 60 s of output, a GPU decode
/// cut libx264's CPU time by 3% on a 10 Mbit/s 1080p source and 20% on a
/// 96 Mbit/s one, and by 17% (25 Mbit/s) to 57% (376 Mbit/s) on a 4K
/// one, though the 4K ones take up to twice as long, the frames' way
/// back from the GPU being slow: still over three times real time.
pub fn decode(spec: &EncodeSpec<'_>) -> Decode {
    match gpu_decodable(spec) {
        None => Decode::Software,
        Some(Some(Hardware::Vaapi)) if cpu_filters(spec).is_empty() => Decode::Gpu,
        Some(_) => Decode::Hardware,
    }
}

/// If `spec`'s video may be decoded on the GPU, what encodes it: `None`
/// for software, or VA-API.
fn gpu_decodable(spec: &EncodeSpec<'_>) -> Option<Option<Hardware>> {
    let (Some(ConvertedVideo::Encode { codec, .. }), Some(stream)) =
        (spec.video, &spec.descriptor.video)
    else {
        return None;
    };
    // NVENC and QSV would decode with their own hwaccels, which are not
    // set up.
    let hardware = spec.encoders.video_encoder(codec)?.hardware;
    let source = codecs::video_codec(&stream.codec);
    (spec.hardware_decode
        && matches!(hardware, None | Some(Hardware::Vaapi))
        && spec.encoders.decodes(source, Hardware::Vaapi)
        && gpu_decodes(stream, source))
    .then_some(hardware)
}

/// A filter of a video encode that runs on the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuFilter {
    /// The GPU's driver has no `tonemap_vaapi`, or GPU filters are off.
    ToneMap,
    /// The GPU's driver has no `overlay_vaapi`, or GPU filters are off.
    Pictures,
    /// Text is rendered by libass, on the CPU only. It is burned in only
    /// for a device that shows no text format at all, so it has no GPU
    /// path.
    Text,
}

/// The filters of `spec`'s video encode that run on the CPU: tone
/// mapping and burn-in, unless GPU filters are on and the driver runs
/// them. Scaling always can run on the GPU.
fn cpu_filters(spec: &EncodeSpec<'_>) -> Vec<CpuFilter> {
    let Some(ConvertedVideo::Encode { tone_map, .. }) = spec.video else {
        return Vec::new();
    };
    let mut cpu = Vec::new();
    if tone_map && !(spec.gpu_filters && spec.encoders.gpu_tonemap) {
        cpu.push(CpuFilter::ToneMap);
    }
    match burned(spec).ok().flatten().map(|track| track.kind) {
        Some(SubtitleKind::Bitmap) if spec.gpu_filters && spec.encoders.gpu_overlay => {}
        Some(SubtitleKind::Bitmap) => cpu.push(CpuFilter::Pictures),
        Some(SubtitleKind::Text) => cpu.push(CpuFilter::Text),
        None => {}
    }
    cpu
}

/// The subtitle track burned in, if one is.
fn burned<'a>(spec: &EncodeSpec<'a>) -> Result<Option<&'a SubtitleStream>, String> {
    match spec.subtitle {
        Some(
            sub @ ConvertedSubtitle {
                action: SubtitleAction::BurnIn { .. },
                ..
            },
        ) => spec
            .descriptor
            .subtitles
            .get(sub.index as usize)
            .map(Some)
            .ok_or_else(|| format!("subtitle {}: no such track", sub.index)),
        _ => Ok(None),
    }
}

/// With GPU filters on, why filters `spec` asks the GPU for run on the
/// CPU instead: the driver lacks one, or the subtitle is text. Nothing
/// when they are off, or the video is not decoded on the GPU at all.
pub fn cpu_instead_of_gpu(spec: &EncodeSpec<'_>) -> Vec<&'static str> {
    if !spec.gpu_filters || gpu_decodable(spec) != Some(Some(Hardware::Vaapi)) {
        return Vec::new();
    }
    cpu_filters(spec)
        .into_iter()
        .map(|filter| match filter {
            CpuFilter::ToneMap => "the GPU's driver does not tone map (tonemap_vaapi)",
            CpuFilter::Pictures => "the GPU's driver does not overlay pictures (overlay_vaapi)",
            CpuFilter::Text => "text subtitles are burned in on the CPU",
        })
        .collect()
}

/// Where one of a video encode's filters runs (PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOn {
    /// Not in the graph.
    None,
    Gpu,
    Cpu,
}

/// What decodes a run's video (PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoder {
    /// Nothing: the video is copied, or there is none.
    None,
    Software,
    /// On the GPU, through this API.
    Hardware(Hardware),
}

/// How a run makes its video (PX-24): what Server › Playback shows,
/// read from the spec its arguments are built from, so it says what
/// [`ffmpeg_args`] built, not what ffmpeg's output claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub decoder: Decoder,
    /// The video encoder, for an encoded video.
    pub encoder: Option<VideoEncoder>,
    pub tone_map: FilterOn,
    pub burn_in: FilterOn,
    /// The picture written, width × height: the source's, or scaled to
    /// the plan's height as `scale=-2:h` (or `scale_vaapi=w=-2`) scales
    /// it. `None` with no video.
    pub size: Option<(u32, u32)>,
    /// The encoder's stated target (`-b:v`), when it has one.
    pub target_bitrate: Option<u64>,
}

/// [`Made`] for `spec`: the same decisions [`ffmpeg_args`] makes.
pub fn made(spec: &EncodeSpec<'_>) -> Made {
    let stream = spec.descriptor.video.as_ref();
    let source = stream.map(|v| (v.width, v.height));
    let Some(ConvertedVideo::Encode {
        codec,
        height,
        max_bitrate,
        tone_map,
    }) = spec.video
    else {
        return Made {
            decoder: Decoder::None,
            encoder: None,
            tone_map: FilterOn::None,
            burn_in: FilterOn::None,
            size: source.filter(|_| matches!(spec.video, Some(ConvertedVideo::Copy { .. }))),
            target_bitrate: None,
        };
    };
    let decode = decode(spec);
    // Every filter is on the GPU or none is: `decode` keeps the frames
    // there only when nothing on the CPU needs them.
    let on = |used: bool| match (used, decode) {
        (false, _) => FilterOn::None,
        (true, Decode::Gpu) => FilterOn::Gpu,
        (true, _) => FilterOn::Cpu,
    };
    Made {
        decoder: match decode {
            Decode::Software => Decoder::Software,
            // Only VA-API decodes on the GPU (`gpu_decodable`).
            Decode::Hardware | Decode::Gpu => Decoder::Hardware(Hardware::Vaapi),
        },
        encoder: spec.encoders.video_encoder(codec).cloned(),
        tone_map: on(tone_map),
        burn_in: on(matches!(burned(spec), Ok(Some(_)))),
        size: source.map(|(w, h)| scaled(w, h, height)),
        target_bitrate: max_bitrate,
    }
}

/// `width` × `height` scaled to `to` high as `scale=-2:to` does: the
/// width kept in proportion, rounded to the nearest even number. A video
/// no taller than `to` is not scaled.
fn scaled(width: u32, height: u32, to: u32) -> (u32, u32) {
    if to >= height || height == 0 {
        return (width, height);
    }
    let half = (u64::from(width) * u64::from(to) + u64::from(height)) / (2 * u64::from(height));
    ((half * 2) as u32, to)
}

/// Whether VA-API drivers decode `stream` at all: 4:2:0, and H.264 only
/// in 8 bits. Detection's test decode is of 8-bit 4:2:0, so it says
/// nothing of the rest. One this lets through that the driver still
/// refuses, ffmpeg decodes in software itself, or the job is run again
/// in software (`job`).
fn gpu_decodes(stream: &VideoStream, codec: VideoCodec) -> bool {
    let four_two_zero = stream.pixel_format.as_deref().is_none_or(|format| {
        ["yuv420p", "yuvj420p", "nv12", "p010"]
            .iter()
            .any(|ok| format.starts_with(ok))
    });
    four_two_zero && !(codec == VideoCodec::H264 && stream.bit_depth > 8)
}

/// HDR to SDR (BT.709) with the Hable curve, in software: it works for any
/// source and any encoder, where `tonemap_vaapi` needs a VA-API decode.
pub(crate) const TONE_MAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
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
        // Not the path itself: the reason reaches the client (PX-09
        // finding 23).
        return Err("a subtitle file's path is not one ffmpeg's filters can take".into());
    }
    Ok(format!("'{raw}'"))
}

fn seconds(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// The video's filters on the CPU: burn-in, tone mapping and scale, and
/// the upload to a VA-API encoder.
fn cpu_graph(
    spec: &EncodeSpec<'_>,
    stream: &VideoStream,
    codec: VideoCodec,
    height: u32,
    tone_map: bool,
    vaapi: bool,
) -> Result<String, String> {
    let d = spec.descriptor;
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
            (source_of, SubtitleKind::Bitmap) => {
                let pictures = pictures_input(source_of);
                // Pictures go over the picture, their canvas scaled to it
                // first: Blu-ray PGS is 1080p even over 4K video, and
                // unscaled it would land small in the top left.
                source = format!(
                    "[{pictures}]scale={}:{}[subs];[0:{}][subs]overlay",
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
            (SubtitleSource::External { path }, SubtitleKind::Text) => {
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
    Ok(if chain.is_empty() {
        format!("{source}null[v]")
    } else if source.ends_with("overlay") {
        format!("{source},{}[v]", chain.join(","))
    } else {
        format!("{source}{}[v]", chain.join(","))
    })
}

/// The video's filters on the GPU, for frames decoded there, in the
/// CPU's order: burned-in pictures laid over it (`overlay_vaapi`, their
/// canvas scaled to the video and uploaded), the tone map to 8-bit SDR
/// ([`TONEMAP_VAAPI`]), and the scale, to an even width as the CPU's
/// `scale=-2` makes it, and to 8 bits, as the CPU's chain uploads `nv12`
/// for every VA-API encode. [`decode`] chose this path, so the GPU runs
/// every filter asked for.
fn gpu_graph(
    spec: &EncodeSpec<'_>,
    stream: &VideoStream,
    height: u32,
    tone_map: bool,
) -> Result<String, String> {
    let mut chain = Vec::new();
    if tone_map {
        chain.push(s(TONEMAP_VAAPI));
    }
    let mut options = Vec::new();
    if height < stream.height {
        options.push(format!("w=-2:h={height}"));
    }
    if stream.bit_depth > 8 && !tone_map {
        options.push(s("format=nv12"));
    }
    if !options.is_empty() {
        chain.push(format!("scale_vaapi={}", options.join(":")));
    }
    let video = format!("[0:{}]", stream.index);
    Ok(match burned(spec)? {
        Some(track) => {
            let mut graph = format!(
                "[{}]scale={}:{},format=rgba,hwupload[subs];{video}[subs]overlay_vaapi",
                pictures_input(&track.source),
                stream.width,
                stream.height
            );
            for filter in chain {
                graph.push(',');
                graph.push_str(&filter);
            }
            graph + "[v]"
        }
        None if chain.is_empty() => format!("{video}null[v]"),
        None => format!("{video}{}[v]", chain.join(",")),
    })
}

/// Where burned-in pictures come from: the film's own stream, or the
/// external file, the second input.
fn pictures_input(source: &SubtitleSource) -> String {
    match source {
        SubtitleSource::Embedded { index } => format!("0:{index}"),
        SubtitleSource::External { .. } => s("1:0"),
    }
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
    let decode = decode(spec);
    // Frames decoded on the GPU and left there bring their device to the
    // encoder; uploaded ones need it named.
    if vaapi && decode != Decode::Gpu {
        args.extend([s("-vaapi_device"), s(spec.vaapi_device.display())]);
    }
    // An option of the film's input, so before its seek.
    if decode == Decode::Gpu && matches!(burned(spec), Ok(Some(_))) {
        // The pictures are uploaded to the device the film decodes on:
        // one device, named, for both.
        args.extend([
            s("-init_hw_device"),
            format!("vaapi=gpu:{}", spec.vaapi_device.display()),
            s("-filter_hw_device"),
            s("gpu"),
        ]);
        args.extend(["-hwaccel", "vaapi", "-hwaccel_device", "gpu"].map(s));
        args.extend(["-hwaccel_output_format", "vaapi"].map(s));
    } else if decode != Decode::Software {
        args.extend(hwaccel_args(
            Hardware::Vaapi,
            spec.vaapi_device,
            decode == Decode::Gpu,
        ));
    }

    let seek = |args: &mut Vec<String>| {
        if let Some(start) = spec.start {
            args.extend([s("-ss"), seconds(start)]);
        }
    };
    seek(&mut args);
    args.extend([s("-i"), s(spec.input.display())]);
    let picked = spec
        .subtitle
        .and_then(|sub| d.subtitles.get(sub.index as usize));
    let sidecar = picked.and_then(|track| match &track.source {
        SubtitleSource::External { path } => {
            Some(spec.sidecar.map_or_else(|| path.clone(), Path::to_path_buf))
        }
        SubtitleSource::Embedded { .. } => None,
    });
    // External pictures to burn in: laid over the video, as embedded ones
    // are. (External text is burned in by a filter that reads the file
    // itself.)
    let burned_pictures = sidecar.is_some()
        && picked.is_some_and(|track| track.kind == SubtitleKind::Bitmap)
        && matches!(
            spec.subtitle,
            Some(ConvertedSubtitle {
                action: SubtitleAction::BurnIn { .. },
                ..
            })
        );
    let mut second_input = false;
    if let (Some(path), Some(sub)) = (&sidecar, spec.subtitle) {
        if burned_pictures {
            // A second input, read from its start: a cue that began
            // before the seek is still drawn, and ffmpeg waits for ever
            // on a picture file sought past its last cue.
            args.extend([s("-i"), s(path.display())]);
            second_input = true;
        } else if !matches!(
            sub.action,
            SubtitleAction::BurnIn { .. } | SubtitleAction::Ocr { .. }
        ) {
            // A second input, sought the same way.
            seek(&mut args);
            args.extend([s("-i"), s(path.display())]);
            second_input = true;
        }
    }
    if spec.start.is_some() || second_input {
        // The film's own time, not 0, so the player shows where it is
        // and HLS segments line up with the playlist and the cues.
        // And for a subtitle file, burned in or kept as a stream: without
        // it ffmpeg moves a picture file's times back by its first cue's,
        // and every cue shows early.
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
                let graph = if decode == Decode::Gpu {
                    gpu_graph(spec, stream, height, tone_map)?
                } else {
                    cpu_graph(spec, stream, codec, height, tone_map, vaapi)?
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
                    // A keyframe at every cut, so HLS cuts land on the grid.
                    args.extend([s("-force_key_frames"), forced_keyframes(from_start(spec))]);
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
            // An encoded video's run from 0 cuts its short head (PX-26):
            // the muxer's thresholds are `hls_init_time` × n until its list
            // is full, then go on by `hls_time` from the list's end. With
            // one entry that is 2 s, 4 s, 8 s, then every 6 s: the grid of
            // `hls::Segments::encoded`. The list is ffmpeg's own playlist,
            // which the server reads only for its last segment (`job`).
            let list_size = match spec.video {
                Some(ConvertedVideo::Encode { .. }) if from_start(spec) => {
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
                        codec: VideoCodec::Hevc
                    } | ConvertedVideo::Encode {
                        codec: VideoCodec::Hevc,
                        ..
                    }
                )
            ) {
                args.extend([s("-tag:v"), s("hvc1")]);
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
                    codec: VideoCodec::Hevc
                })
            ) {
                args.extend([s("-bsf:v"), s("hevc_mp4toannexb")]);
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
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::encoders::{AudioEncoder, HardwareDecoder};
    use spoolframe_media::{AudioStream, SubtitleStream, VideoStream};
    use spoolframe_types::{AudioCodec, AudioRole, HdrFormat, SubtitleRole};

    use super::*;

    /// A 4K HDR10 HEVC MKV: stream 0 video, 1 DTS 5.1, 2 PGS, 3 SRT, and an
    /// external SRT and an external PGS file.
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
                level: Some(153),
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
                sub(
                    SubtitleSource::External {
                        path: "/media/Film.en.sup".into(),
                    },
                    "hdmv_pgs_subtitle",
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
            gpu_tonemap: false,
            gpu_overlay: false,
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
            hardware_decode: true,
            gpu_filters: false,
            start: None,
            output,
        }
    }

    /// `encoders(vaapi)`, with VA-API decoders for H.264, HEVC and AV1,
    /// as on the dev machine.
    fn with_decoders(vaapi: bool) -> Encoders {
        Encoders {
            hardware_decoders: [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]
                .map(|codec| HardwareDecoder {
                    codec,
                    hardware: Hardware::Vaapi,
                })
                .to_vec(),
            ..encoders(vaapi)
        }
    }

    /// An H.264 encode to 1080p, tone-mapped: on the CPU whatever decodes.
    const TONE_MAPPED_H264: ConvertedVideo = ConvertedVideo::Encode {
        codec: VideoCodec::H264,
        height: 1080,
        max_bitrate: None,
        tone_map: true,
    };

    fn position(args: &[String], flag: &str) -> usize {
        args.iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag} in {args:?}"))
    }

    #[test]
    fn a_vaapi_encode_decodes_on_the_gpu_into_memory_where_a_decoder_was_found() {
        let (film, enc) = (film(), with_decoders(true));
        let mut spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        spec.start = Some(Duration::from_secs(600));
        assert_eq!(decode(&spec), Decode::Hardware);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(
            has(
                &args,
                &[
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_device",
                    "/dev/dri/renderD128"
                ]
            ),
            "{args:?}"
        );
        // The frames come back to memory, for the filters on the CPU.
        assert!(!args.contains(&s("-hwaccel_output_format")), "{args:?}");
        // An input option: before the seek and the film.
        assert!(position(&args, "-hwaccel") < position(&args, "-ss"));
        assert!(position(&args, "-hwaccel") < position(&args, "-i"));
        // The chain is as it was: tone-mapped, scaled and uploaded on the
        // CPU's side.
        let graph = after(&args, "-filter_complex");
        assert!(graph.contains("tonemap=tonemap=hable"), "{graph}");
        assert!(graph.ends_with("format=nv12,hwupload[v]"), "{graph}");
    }

    #[test]
    fn a_codec_without_a_hardware_decoder_decodes_in_software() {
        let film = film();
        // Detection found no decoder at all.
        let enc = encoders(true);
        let none = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        assert_eq!(decode(&none), Decode::Software);
        assert!(!ffmpeg_args(&none).unwrap().contains(&s("-hwaccel")));
        // Nor for this codec.
        let enc = Encoders {
            hardware_decoders: vec![HardwareDecoder {
                codec: VideoCodec::H264,
                hardware: Hardware::Vaapi,
            }],
            ..encoders(true)
        };
        let spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Software);
    }

    /// A libx264 encode decodes on the GPU too, into memory, where it
    /// encodes: it takes no VA-API device of its own.
    #[test]
    fn a_software_encode_decodes_on_the_gpu_into_memory() {
        let (film, enc) = (film(), with_decoders(false));
        // Not even a filter on the CPU keeps frames on the GPU for it.
        let spec = spec(&film, &enc, plain_h264(1080), COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Hardware);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(
            has(
                &args,
                &[
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_device",
                    "/dev/dri/renderD128",
                    "-i"
                ]
            ),
            "{args:?}"
        );
        assert!(!args.contains(&s("-vaapi_device")), "{args:?}");
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale=-2:1080,format=yuv420p[v]"
        );
        assert!(has(&args, &["-c:v", "libx264"]));
    }

    /// NVENC would decode with CUDA, which nothing here sets up.
    #[test]
    fn an_nvenc_encode_decodes_in_software() {
        let film = film();
        let mut enc = with_decoders(true);
        for encoder in &mut enc.video {
            encoder.hardware = Some(Hardware::Nvenc);
            encoder.name = encoder.name.replace("vaapi", "nvenc");
        }
        let spec = spec(&film, &enc, plain_h264(1080), COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Software);
        assert!(!ffmpeg_args(&spec).unwrap().contains(&s("-hwaccel")));
    }

    #[test]
    fn a_file_whose_hardware_decode_failed_decodes_in_software() {
        let (film, enc) = (film(), with_decoders(true));
        let mut spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        spec.hardware_decode = false;
        assert_eq!(decode(&spec), Decode::Software);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(!args.contains(&s("-hwaccel")), "{args:?}");
        // Still encoded on the GPU.
        assert!(has(&args, &["-c:v", "h264_vaapi"]));
    }

    #[test]
    fn a_copied_video_is_not_decoded() {
        let (film, enc) = (film(), with_decoders(true));
        let spec = spec(&film, &enc, COPY_HEVC, COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Software);
        assert!(!ffmpeg_args(&spec).unwrap().contains(&s("-hwaccel")));
    }

    /// No VA-API driver decodes 10-bit H.264 or 4:2:2: not tried.
    /// An H.264 encode to `height`, nothing more.
    fn plain_h264(height: u32) -> ConvertedVideo {
        ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height,
            max_bitrate: Some(8_000_000),
            tone_map: false,
        }
    }

    /// `film()` as an SDR 1080p 8-bit H.264 file.
    fn sdr_1080p() -> MediaDescriptor {
        let mut film = film();
        let video = film.video.as_mut().unwrap();
        video.codec = "h264".into();
        video.profile = Some("High".into());
        (video.width, video.height) = (1920, 1080);
        video.pixel_format = Some("yuv420p".into());
        video.bit_depth = 8;
        video.color_transfer = None;
        video.hdr = None;
        film
    }

    #[test]
    fn a_vaapi_encode_with_no_filter_on_the_cpu_stays_on_the_gpu() {
        let enc = with_decoders(true);
        // 4K 10-bit HEVC to 1080p H.264, not tone-mapped.
        let film = film();
        let spec = spec(&film, &enc, plain_h264(1080), COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Gpu);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(
            has(
                &args,
                &[
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_device",
                    "/dev/dri/renderD128",
                    "-hwaccel_output_format",
                    "vaapi",
                    "-i",
                    "/media/Film.mkv"
                ]
            ),
            "{args:?}"
        );
        // Scaled on the GPU to an even width, and 8-bit as the CPU's
        // chain makes it; no upload, the frames are there.
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale_vaapi=w=-2:h=1080:format=nv12[v]"
        );
        assert!(has(&args, &["-map", "[v]", "-c:v", "h264_vaapi"]));
        // The encoder takes its device from the frames.
        assert!(!args.contains(&s("-vaapi_device")), "{args:?}");
    }

    #[test]
    fn on_the_gpu_a_video_kept_at_its_height_is_only_converted_to_8_bits() {
        let enc = with_decoders(true);
        let film = film();
        let same = spec(&film, &enc, plain_h264(2160), COPY_DTS, None, hls());
        assert_eq!(decode(&same), Decode::Gpu);
        assert_eq!(
            after(&ffmpeg_args(&same).unwrap(), "-filter_complex"),
            "[0:0]scale_vaapi=format=nv12[v]"
        );
        let sdr = sdr_1080p();
        let plain = spec(&sdr, &enc, plain_h264(1080), COPY_DTS, None, hls());
        assert_eq!(decode(&plain), Decode::Gpu);
        assert_eq!(
            after(&ffmpeg_args(&plain).unwrap(), "-filter_complex"),
            "[0:0]null[v]"
        );
        let smaller = spec(&sdr, &enc, plain_h264(720), COPY_DTS, None, hls());
        assert_eq!(
            after(&ffmpeg_args(&smaller).unwrap(), "-filter_complex"),
            "[0:0]scale_vaapi=w=-2:h=720[v]"
        );
    }

    /// Tone mapping and burn-in run on the CPU: the frames come back to
    /// memory for them.
    #[test]
    fn a_filter_on_the_cpu_keeps_the_gpu_decode_in_memory() {
        let enc = with_decoders(true);
        let film = film();
        let tone_mapped = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        assert_eq!(decode(&tone_mapped), Decode::Hardware);
        let burn = |index| ConvertedSubtitle {
            index,
            action: SubtitleAction::BurnIn {
                format: SubtitleFormat::Pgs,
            },
        };
        for index in [0, 1] {
            let burned = spec(
                &film,
                &enc,
                plain_h264(1080),
                COPY_DTS,
                Some(burn(index)),
                hls(),
            );
            assert_eq!(decode(&burned), Decode::Hardware, "subtitle {index}");
            let args = ffmpeg_args(&burned).unwrap();
            assert!(!args.contains(&s("-hwaccel_output_format")), "{args:?}");
            assert!(after(&args, "-filter_complex").ends_with("format=nv12,hwupload[v]"));
        }
        // A subtitle kept as a stream needs no filter.
        let kept = ConvertedSubtitle {
            index: 1,
            action: SubtitleAction::Copy {
                format: SubtitleFormat::Srt,
            },
        };
        let spec = spec(
            &film,
            &enc,
            plain_h264(1080),
            COPY_DTS,
            Some(kept),
            progressive(),
        );
        assert_eq!(decode(&spec), Decode::Gpu);
    }

    /// What runs when the GPU path fails: the CPU's chain, decoded in
    /// software, still encoded on the GPU.
    #[test]
    fn the_gpu_path_s_fallback_is_the_cpu_chain() {
        let (film, enc) = (film(), with_decoders(true));
        let mut spec = spec(&film, &enc, plain_h264(1080), COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Gpu);
        spec.hardware_decode = false;
        assert_eq!(decode(&spec), Decode::Software);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(!args.contains(&s("-hwaccel")), "{args:?}");
        assert_eq!(after(&args, "-vaapi_device"), "/dev/dri/renderD128");
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale=-2:1080,format=nv12,hwupload[v]"
        );
    }

    /// `with_decoders(true)` on a driver with these GPU filters.
    fn with_filters(tonemap: bool, overlay: bool) -> Encoders {
        Encoders {
            gpu_tonemap: tonemap,
            gpu_overlay: overlay,
            ..with_decoders(true)
        }
    }

    fn burn_in(index: u32, format: SubtitleFormat) -> Option<ConvertedSubtitle> {
        Some(ConvertedSubtitle {
            index,
            action: SubtitleAction::BurnIn { format },
        })
    }

    #[test]
    fn with_gpu_filters_off_the_cpu_tone_maps_where_the_gpu_could() {
        let (film, enc) = (film(), with_filters(true, true));
        let spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Hardware);
        let graph = s(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"));
        assert!(graph.contains("tonemap=tonemap=hable"), "{graph}");
        assert!(!graph.contains("tonemap_vaapi"), "{graph}");
        assert!(cpu_instead_of_gpu(&spec).is_empty());
        // Nor pictures.
        let pgs = burn_in(0, SubtitleFormat::Pgs);
        let burned = spec_with(&film, &enc, plain_h264(1080), pgs);
        assert_eq!(decode(&burned), Decode::Hardware);
        let graph = s(after(&ffmpeg_args(&burned).unwrap(), "-filter_complex"));
        assert!(
            graph.contains("overlay,") && !graph.contains("overlay_vaapi"),
            "{graph}"
        );
    }

    fn spec_with<'a>(
        film: &'a MediaDescriptor,
        encoders: &'a Encoders,
        video: ConvertedVideo,
        subtitle: Option<ConvertedSubtitle>,
    ) -> EncodeSpec<'a> {
        spec(film, encoders, video, COPY_DTS, subtitle, hls())
    }

    #[test]
    fn with_gpu_filters_on_the_gpu_tone_maps_where_its_driver_can() {
        let (film, enc) = (film(), with_filters(true, false));
        let mut spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        spec.gpu_filters = true;
        assert_eq!(decode(&spec), Decode::Gpu);
        let args = ffmpeg_args(&spec).unwrap();
        assert_eq!(after(&args, "-hwaccel_output_format"), "vaapi");
        // To 8-bit SDR, then scaled.
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709,\
             scale_vaapi=w=-2:h=1080[v]"
        );
        assert!(!args.contains(&s("hwupload")));
        assert!(cpu_instead_of_gpu(&spec).is_empty());
        // At its own height, only tone-mapped.
        let mut same = spec.clone();
        same.video = Some(ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 2160,
            max_bitrate: None,
            tone_map: true,
        });
        assert_eq!(
            after(&ffmpeg_args(&same).unwrap(), "-filter_complex"),
            "[0:0]tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709[v]"
        );
    }

    #[test]
    fn with_gpu_filters_on_a_driver_that_cannot_tone_map_leaves_it_to_the_cpu() {
        let (film, enc) = (film(), with_filters(false, true));
        let mut spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        spec.gpu_filters = true;
        assert_eq!(decode(&spec), Decode::Hardware);
        let graph = s(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"));
        assert!(graph.contains("tonemap=tonemap=hable"), "{graph}");
        assert_eq!(
            cpu_instead_of_gpu(&spec),
            ["the GPU's driver does not tone map (tonemap_vaapi)"]
        );
    }

    #[test]
    fn with_gpu_filters_on_pictures_are_burned_in_on_the_gpu_where_its_driver_can() {
        let (film, enc) = (film(), with_filters(false, true));
        // Embedded PGS, then an external one: the second input.
        for (index, pictures) in [(0, "0:2"), (3, "1:0")] {
            let mut spec = spec(
                &film,
                &enc,
                plain_h264(1080),
                COPY_DTS,
                burn_in(index, SubtitleFormat::Pgs),
                progressive(),
            );
            spec.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
            spec.gpu_filters = true;
            assert_eq!(decode(&spec), Decode::Gpu, "subtitle {index}");
            let args = ffmpeg_args(&spec).unwrap();
            // One device for the decode and the pictures' upload.
            assert!(
                has(
                    &args,
                    &[
                        "-init_hw_device",
                        "vaapi=gpu:/dev/dri/renderD128",
                        "-filter_hw_device",
                        "gpu",
                        "-hwaccel",
                        "vaapi",
                        "-hwaccel_device",
                        "gpu",
                        "-hwaccel_output_format",
                        "vaapi"
                    ]
                ),
                "{args:?}"
            );
            // The pictures' canvas scaled to the video, laid over it
            // before it is scaled, as on the CPU.
            assert_eq!(
                after(&args, "-filter_complex"),
                format!(
                    "[{pictures}]scale=3840:2160,format=rgba,hwupload[subs];\
                     [0:0][subs]overlay_vaapi,scale_vaapi=w=-2:h=1080:format=nv12[v]"
                )
            );
            assert!(cpu_instead_of_gpu(&spec).is_empty());
        }
    }

    #[test]
    fn with_gpu_filters_on_text_and_what_the_driver_lacks_stay_on_the_cpu() {
        let film = film();
        let all = with_filters(true, true);
        let text = burn_in(1, SubtitleFormat::Srt);
        let mut spec_text = spec(&film, &all, plain_h264(1080), COPY_DTS, text, hls());
        spec_text.gpu_filters = true;
        assert_eq!(decode(&spec_text), Decode::Hardware);
        assert!(
            after(&ffmpeg_args(&spec_text).unwrap(), "-filter_complex")
                .contains("subtitles=filename=")
        );
        assert_eq!(
            cpu_instead_of_gpu(&spec_text),
            ["text subtitles are burned in on the CPU"]
        );
        // Pictures on a driver that cannot overlay: tone map and burn-in
        // both on the CPU, the frames being there anyway.
        let no_overlay = with_filters(true, false);
        let pgs = burn_in(0, SubtitleFormat::Pgs);
        let mut spec = spec(&film, &no_overlay, TONE_MAPPED_H264, COPY_DTS, pgs, hls());
        spec.gpu_filters = true;
        assert_eq!(decode(&spec), Decode::Hardware);
        let graph = s(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"));
        assert!(graph.contains("overlay,") && graph.contains("tonemap=tonemap=hable"));
        assert_eq!(
            cpu_instead_of_gpu(&spec),
            ["the GPU's driver does not overlay pictures (overlay_vaapi)"]
        );
    }

    /// PX-24: a VA-API encode with GPU filters on a driver that runs both
    /// says the tone map and the burn-in are on the GPU, as the graph has
    /// them.
    #[test]
    fn made_says_a_gpu_tone_map_and_a_gpu_burn_in() {
        let (film, enc) = (film(), with_filters(true, true));
        let mut spec = spec_with(
            &film,
            &enc,
            TONE_MAPPED_H264,
            burn_in(0, SubtitleFormat::Pgs),
        );
        spec.gpu_filters = true;
        let graph = s(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"));
        assert!(graph.contains("tonemap_vaapi") && graph.contains("overlay_vaapi"));
        let made = made(&spec);
        assert_eq!(made.decoder, Decoder::Hardware(Hardware::Vaapi));
        assert_eq!(made.encoder.map(|e| e.name), Some(s("h264_vaapi")));
        assert_eq!(
            (made.tone_map, made.burn_in),
            (FilterOn::Gpu, FilterOn::Gpu)
        );
        assert_eq!(made.size, Some((1920, 1080)));
    }

    /// A driver that tone maps but cannot overlay: the pictures are laid
    /// over on the CPU, so the frames come back and the tone map is done
    /// there too. What the graph does, not what the driver could.
    #[test]
    fn made_says_a_cpu_burn_in_takes_the_tone_map_to_the_cpu() {
        let (film, enc) = (film(), with_filters(true, false));
        let mut spec = spec_with(
            &film,
            &enc,
            TONE_MAPPED_H264,
            burn_in(0, SubtitleFormat::Pgs),
        );
        spec.gpu_filters = true;
        let graph = s(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"));
        assert!(graph.contains("tonemap=tonemap=hable") && graph.contains("overlay,"));
        let made = made(&spec);
        assert_eq!(made.decoder, Decoder::Hardware(Hardware::Vaapi));
        assert_eq!(
            (made.tone_map, made.burn_in),
            (FilterOn::Cpu, FilterOn::Cpu)
        );
    }

    /// NVENC decodes in software here, and filters on the CPU.
    #[test]
    fn made_says_nvenc_decodes_in_software() {
        let film = film();
        let enc = Encoders {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "h264_nvenc".into(),
                hardware: Some(Hardware::Nvenc),
            }],
            ..with_decoders(false)
        };
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 720,
            max_bitrate: Some(4_000_000),
            tone_map: true,
        };
        let made = made(&spec_with(&film, &enc, video, None));
        assert_eq!(made.decoder, Decoder::Software);
        assert_eq!(
            made.encoder.map(|e| (e.name, e.hardware)),
            Some((s("h264_nvenc"), Some(Hardware::Nvenc)))
        );
        assert_eq!(
            (made.tone_map, made.burn_in),
            (FilterOn::Cpu, FilterOn::None)
        );
        assert_eq!(made.size, Some((1280, 720)));
        assert_eq!(made.target_bitrate, Some(4_000_000));
    }

    /// A copied video is neither decoded nor encoded nor filtered: it is
    /// written at its own size.
    #[test]
    fn made_says_a_copy_runs_nothing() {
        let (film, enc) = (film(), with_filters(true, true));
        let made = made(&spec_with(&film, &enc, COPY_HEVC, None));
        assert_eq!(
            made,
            Made {
                decoder: Decoder::None,
                encoder: None,
                tone_map: FilterOn::None,
                burn_in: FilterOn::None,
                size: Some((3840, 2160)),
                target_bitrate: None,
            }
        );
    }

    #[test]
    fn a_scaled_width_is_even_and_in_proportion() {
        assert_eq!(scaled(3840, 2160, 1080), (1920, 1080));
        assert_eq!(scaled(1920, 800, 720), (1728, 720));
        // 1918 × 720 / 1080 = 1278.7: 639.3 pairs, rounded to 639.
        assert_eq!(scaled(1918, 1080, 720), (1278, 720));
        assert_eq!(scaled(1280, 720, 1080), (1280, 720));
    }

    #[test]
    fn streams_no_gpu_decodes_are_decoded_in_software() {
        let enc = with_decoders(true);
        let mut high10 = film();
        let video = high10.video.as_mut().unwrap();
        video.codec = "h264".into();
        video.profile = Some("High 10".into());
        let spec_of = |film| spec(film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        assert_eq!(decode(&spec_of(&high10)), Decode::Software);
        let mut four_two_two = film();
        four_two_two.video.as_mut().unwrap().pixel_format = Some("yuv422p10le".into());
        assert_eq!(decode(&spec_of(&four_two_two)), Decode::Software);
        // 10-bit HEVC is decoded.
        assert_eq!(decode(&spec_of(&film())), Decode::Hardware);
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
        assert_eq!(
            after(&args, "-hls_segment_options"),
            "movflags=+frag_discont"
        );
        // Copied video keeps the source's keyframes.
        assert!(!args.contains(&s("-force_key_frames")));
    }

    fn hls() -> Output {
        Output::Hls {
            dir: "/scratch/h/0".into(),
            start_number: 1,
        }
    }

    #[test]
    fn hevc_in_hls_is_tagged_hvc1_copied_or_encoded() {
        let film = film();
        let args = ffmpeg_args(&spec(
            &film,
            &encoders(false),
            COPY_HEVC,
            COPY_DTS,
            None,
            hls(),
        ))
        .unwrap();
        assert!(has(&args, &["-tag:v", "hvc1"]), "{args:?}");
        // Whatever encodes it: ffmpeg writes `hev1` for all of them.
        let encode = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        };
        for vaapi in [false, true] {
            let enc = encoders(vaapi);
            let args = ffmpeg_args(&spec(&film, &enc, encode, COPY_DTS, None, hls())).unwrap();
            assert!(has(&args, &["-tag:v", "hvc1"]), "{args:?}");
        }
    }

    #[test]
    fn only_copied_hevc_in_hls_has_its_parameter_sets_read_from_the_stream() {
        let (film, enc) = (film(), encoders(false));
        let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, hls())).unwrap();
        assert_eq!(after(&args, "-bsf:v"), "hevc_mp4toannexb", "{args:?}");
        // An encoder gives its own; Matroska keeps the source's record.
        let encode = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        };
        let args = ffmpeg_args(&spec(&film, &enc, encode, COPY_DTS, None, hls())).unwrap();
        assert!(!args.contains(&s("-bsf:v")), "{args:?}");
        let args =
            ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, progressive())).unwrap();
        assert!(!args.contains(&s("-bsf:v")), "{args:?}");
    }

    #[test]
    fn only_hevc_in_hls_is_tagged() {
        let (film, enc) = (film(), encoders(false));
        // Matroska has no sample entries.
        let args =
            ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, progressive())).unwrap();
        assert!(!args.contains(&s("-tag:v")), "{args:?}");
        let h264 = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: None,
            tone_map: true,
        };
        let args = ffmpeg_args(&spec(&film, &enc, h264, COPY_DTS, None, hls())).unwrap();
        assert!(!args.contains(&s("-tag:v")), "{args:?}");
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
    fn an_external_pgs_burn_in_overlays_the_file_s_pictures() {
        let (film, enc) = (film(), encoders(false));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 2160,
            max_bitrate: None,
            tone_map: false,
        };
        let burn = ConvertedSubtitle {
            index: 3,
            action: SubtitleAction::BurnIn {
                format: SubtitleFormat::Pgs,
            },
        };
        let mut spec = spec(&film, &enc, video, COPY_DTS, Some(burn), progressive());
        spec.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
        let args = ffmpeg_args(&spec).unwrap();
        // The file is the second input, and its times are kept.
        assert!(has(
            &args,
            &[
                "-i",
                "/media/Film.mkv",
                "-i",
                "/scratch/h/0/sidecar",
                "-copyts"
            ]
        ));
        assert_eq!(
            after(&args, "-filter_complex"),
            "[1:0]scale=3840:2160[subs];[0:0][subs]overlay,format=yuv420p[v]"
        );
        assert!(!args.contains(&s("-c:s")));

        // At a seek only the film is sought.
        spec.start = Some(Duration::from_secs(1800));
        let args = ffmpeg_args(&spec).unwrap();
        assert!(has(
            &args,
            &[
                "-ss",
                "1800.000",
                "-i",
                "/media/Film.mkv",
                "-i",
                "/scratch/h/0/sidecar",
                "-copyts"
            ]
        ));
    }

    #[test]
    fn an_external_pgs_copy_is_a_second_input_with_its_times_kept() {
        let (film, enc) = (film(), encoders(false));
        let copy = ConvertedSubtitle {
            index: 3,
            action: SubtitleAction::Copy {
                format: SubtitleFormat::Pgs,
            },
        };
        let mut spec = spec(&film, &enc, COPY_HEVC, COPY_DTS, Some(copy), progressive());
        spec.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
        let args = ffmpeg_args(&spec).unwrap();
        // From the start too: there is no seek to ask for it.
        assert!(has(
            &args,
            &[
                "-i",
                "/media/Film.mkv",
                "-i",
                "/scratch/h/0/sidecar",
                "-copyts"
            ]
        ));
        assert!(has(&args, &["-map", "1:0", "-c:s", "copy"]));
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

    fn encoded_hls(start: Option<Duration>, start_number: u32) -> Vec<String> {
        let (film, enc) = (film(), encoders(false));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: None,
            tone_map: true,
        };
        let output = Output::Hls {
            dir: "/scratch/h/0".into(),
            start_number,
        };
        let mut spec = spec(&film, &enc, video, COPY_DTS, None, output);
        spec.start = start;
        ffmpeg_args(&spec).unwrap()
    }

    /// PX-26: a run from 0 forces keyframes at 0, 2, 4, 8, 14… s from its
    /// first frame, and has the muxer cut there: 2 s twice, then 6 s from
    /// the end of a one-entry list.
    #[test]
    fn an_encoded_hls_run_from_the_start_cuts_the_short_head() {
        for start in [None, Some(Duration::ZERO)] {
            let args = encoded_hls(start, 1);
            assert_eq!(
                after(&args, "-force_key_frames"),
                "expr:gte(t,if(lt(n_forced,3),2*n_forced,2+6*(n_forced-2)))"
            );
            assert_eq!(after(&args, "-hls_time"), "6");
            assert_eq!(after(&args, "-hls_init_time"), "2");
            assert_eq!(after(&args, "-hls_list_size"), "1");
        }
    }

    /// A run from segment 4 on is on the 6 s grid from where it starts.
    #[test]
    fn an_encoded_hls_restart_forces_a_keyframe_every_six_seconds() {
        let args = encoded_hls(Some(Duration::from_secs(8)), 4);
        assert_eq!(after(&args, "-force_key_frames"), "expr:gte(t,6*n_forced)");
        assert_eq!(after(&args, "-hls_time"), "6");
        assert!(!args.contains(&s("-hls_init_time")), "{args:?}");
        assert_eq!(after(&args, "-hls_list_size"), "0");
    }

    /// A copied video is cut at its own keyframes: no head of its own.
    #[test]
    fn a_copied_hls_video_has_no_short_head() {
        let (film, enc) = (film(), encoders(false));
        let output = Output::Hls {
            dir: "/scratch/h/0".into(),
            start_number: 1,
        };
        let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, output)).unwrap();
        assert!(!args.contains(&s("-hls_init_time")), "{args:?}");
        assert!(!args.contains(&s("-force_key_frames")), "{args:?}");
        assert_eq!(after(&args, "-hls_list_size"), "0");
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
