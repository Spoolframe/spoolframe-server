//! ffmpeg's arguments for a converted plan (P2-03).
//!
//! Filters run on the CPU and the frames go to the encoder after them, so
//! scaling, tone mapping and burn-in work with any encoder; a VA-API
//! encoder gets its frames uploaded at the end of the chain. The video is
//! decoded on the GPU where it can be ([`decode`]), the frames coming
//! back to memory for those filters, or staying on the GPU for a VA-API
//! or NVENC encode whose filters all run there. An HDR source's tone map
//! may run on the GPU with libplacebo (PGPU-04, [`libplacebo`]), which
//! scales too, through Vulkan.

use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_media::encoders::{
    Encoders, Hardware, NVIDIA_VULKAN, TONEMAP_VAAPI, ToneMapSetting, ToneMapper, VideoEncoder,
    VulkanDevice, cuda_device_args, hwaccel_args, libplacebo_filter,
};
use spoolframe_media::sandbox::{GpuUse, Vulkan};
use spoolframe_media::{
    MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, VideoStream, codecs,
};
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, SubtitleAction,
};
use spoolframe_types::{HdrFormat, SubtitleFormat, VideoCodec};

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

/// The most of `-force_key_frames`' list one run is given, in bytes: one
/// argument is held to 128 KiB by Linux (`MAX_ARG_STRLEN`). A two-hour
/// film keyed every second is about 60 KiB from its start; a run of a
/// longer or denser one stops forcing past the list's end, and a player
/// switching rungs there may land a little off until the next seek.
const MAX_KEYFRAME_LIST: usize = 120 * 1024;

/// `-force_key_frames` at `cuts` (the film's times, ms) from the run's
/// start on. The times are the output's: the film's own after a seek
/// (`-copyts`), and from 0 for a run from the start, as the copy's are.
fn keyframes_at(cuts: &[u64], start: Option<Duration>) -> String {
    forced_list(cuts, start).0
}

/// The last of `cuts` a run from `start` forces a keyframe at, when its
/// list ([`keyframes_at`]) stops short of the last cut: past it the run
/// cuts where its encoder keys, not where the playlist says, and its
/// segments there are not to be read (`Conversion::job_for_segment`
/// starts another run, with a list of its own). `None` when the list
/// holds every cut from `start` on.
pub(crate) fn forced_through(cuts: &[u64], start: Option<Duration>) -> Option<u64> {
    forced_list(cuts, start).1
}

fn forced_list(cuts: &[u64], start: Option<Duration>) -> (String, Option<u64>) {
    let from = start.map_or(0, |d| d.as_millis() as u64);
    let mut list = String::new();
    let mut last = None;
    for &ms in cuts.iter().filter(|&&ms| ms >= from) {
        let entry = format!("{}.{:03}", ms / 1000, ms % 1000);
        if list.len() + entry.len() + 1 > MAX_KEYFRAME_LIST {
            return (list, last);
        }
        if !list.is_empty() {
            list.push(',');
        }
        list.push_str(&entry);
        last = Some(ms);
    }
    (list, None)
}

/// What keeps `encoder` from adding keyframes of its own between the
/// forced ones: a scene cut or a GOP's end would be a cut the playlist
/// (or the copy) does not have. The GOP is long (40 s at 25 fps) rather
/// than unbounded: a source keyed less often than that is cut where the
/// copy is not. Each encoder in its own words: ffmpeg's `-sc_threshold`
/// reaches libx264 alone, and every other encoder reports it unused.
///
/// - libx265: `scenecut=0`, and `open-gop=0`. Its forced keyframes are
///   otherwise CRA pictures (`-forced-idr` notwithstanding), whose
///   leading pictures refer to the segment before: a player starting at
///   one, after a switch or a seek, cannot decode them.
/// - libsvtav1: `scd=0`, its scene change detection (off by default).
/// - NVENC: `-no-scenecut`, and IDR at the forced ones.
/// - VA-API and QSV add no keyframe at a scene change (measured on a clip
///   cut every 1.5 s): the long GOP is all they need.
fn only_forced_keyframes(encoder: &str) -> Vec<String> {
    let mut args = vec![s("-g"), s("1000")];
    match encoder {
        "libx264" => args.extend([s("-sc_threshold"), s("0")]),
        "libx265" => args.extend([s("-x265-params"), s("scenecut=0:open-gop=0")]),
        "libsvtav1" => args.extend([s("-svtav1-params"), s("scd=0")]),
        nvenc if nvenc.ends_with("_nvenc") => {
            args.extend([s("-no-scenecut"), s("1"), s("-forced-idr"), s("1")]);
        }
        _ => {}
    }
    args
}

/// For an HLS run of an encoded video from a seek, a copied audio track
/// cut to start where the video does. A seek lands the demuxer on the
/// source's keyframe before the time, and a copied track starts there,
/// up to a source GOP early (1.6 s in American Sniper, 10 s in a film
/// keyed that rarely), while the decoded video starts at the time. The
/// HLS muxer measures its cuts from the first packet it gets, the audio:
/// its thresholds came that much early, so a cut landed on whatever
/// keyframe came first past one, and with a lead over a segment's length
/// the run began with a segment of audio alone and every number after it
/// was one off the playlist. The `noise` filter drops the packets before
/// the time: the rest keep their own times (`-copyts`), where an output
/// `-ss` would move them back to 0.
fn copied_audio_from_start(spec: &EncodeSpec<'_>) -> Vec<String> {
    let encoded = matches!(spec.video, Some(ConvertedVideo::Encode { .. }));
    match (&spec.output, spec.start) {
        (Output::Hls { .. }, Some(start)) if encoded && !start.is_zero() => vec![
            s("-bsf:a"),
            // `\,`: a bare comma would end the filter's options.
            format!("noise=drop=lt(pts*tb\\,{})", seconds(start)),
        ],
        _ => Vec::new(),
    }
}

/// Where and how the job writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// One Matroska file, readable while it grows (P2-04).
    Progressive { file: PathBuf },
    /// fMP4 segments `seg<n>.m4s` and `init.mp4` in `dir`, numbered from
    /// `start_number` (P2-05). `cuts`: where an encode is to cut instead of
    /// its own grid, the film's times in milliseconds (a remux ladder's
    /// lower rung, PX-21); `None` for its own grid.
    Hls {
        dir: PathBuf,
        start_number: u32,
        cuts: Option<Vec<u64>>,
    },
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
    /// The video may be decoded, and tone mapped by libplacebo, on the
    /// GPU: false once that failed for this file (see [`decode`]).
    pub hardware_decode: bool,
    /// Run the filters on the GPU where it can (`transcode.gpu_filters`):
    /// VA-API's tone map and burn-in, NVIDIA's scale and burn-in (PGPU-03).
    /// Under `tone_map = "auto"`, the tone map on the GPU too.
    pub gpu_filters: bool,
    /// How HDR is tone mapped (`transcode.tone_map`, PGPU-04).
    pub tone_map: ToneMapSetting,
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
    /// On the GPU, and the frames stay there: filtered on it and encoded
    /// from it, for a video that needs no filter on the CPU.
    Gpu,
}

/// Where `spec`'s video is decoded: on the GPU [`placement`] picked for
/// it, unless that failed for this file before; and with the frames left
/// on the GPU when that GPU's encoder takes them there and no filter on
/// the CPU needs them: on VA-API, which scales there always; on NVIDIA
/// only with GPU filters on and `scale_cuda` found ([`cuda_frames`]).
///
/// A software encode gains too. Measured for 60 s of output, a GPU decode
/// cut libx264's CPU time by 3% on a 10 Mbit/s 1080p source and 20% on a
/// 96 Mbit/s one, and by 17% (25 Mbit/s) to 57% (376 Mbit/s) on a 4K
/// one, though the 4K ones take up to twice as long, the frames' way
/// back from the GPU being slow: still over three times real time.
pub fn decode(spec: &EncodeSpec<'_>) -> Decode {
    if let Some((family, device)) = libplacebo(spec) {
        return libplacebo_decode(spec, family, device);
    }
    match placement(spec) {
        Some(Placement {
            decoder: Some(Hardware::Vaapi),
            encoder:
                VideoEncoder {
                    hardware: Some(Hardware::Vaapi),
                    ..
                },
        }) if cpu_filters(spec, Hardware::Vaapi).is_empty() => Decode::Gpu,
        Some(Placement {
            decoder: Some(Hardware::Nvenc),
            encoder:
                VideoEncoder {
                    hardware: Some(Hardware::Nvenc),
                    ..
                },
        }) if cuda_frames(spec) => Decode::Gpu,
        Some(Placement {
            decoder: Some(_), ..
        }) => Decode::Hardware,
        _ => Decode::Software,
    }
}

/// The GPU API `spec`'s video decodes through, if it decodes on one:
/// VA-API, or NVDEC (`Hardware::Nvenc`, `-hwaccel cuda`).
///
/// NVDEC's frames come back to memory (no `-hwaccel_output_format
/// cuda`) for filters on the CPU, NVENC uploading them as it does after
/// a software decode; they stay on the GPU where [`cuda_frames`] says.
/// QSV decodes in software: its hwaccel is not set up.
pub fn gpu_decoder(spec: &EncodeSpec<'_>) -> Option<Hardware> {
    placement(spec).and_then(|place| place.decoder)
}

/// What runs a video encode (PGPU-02): its encoder, and the GPU API that
/// decodes its source, if one does. One family at most: the decoder is
/// the encoder's own family or, for a software encode, any one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement<'a> {
    pub encoder: &'a VideoEncoder,
    pub decoder: Option<Hardware>,
}

impl Placement<'_> {
    /// The GPU the run may open: the encoder's, or for a software encode
    /// the decoder's; none for software throughout.
    pub fn gpu(&self) -> GpuUse {
        self.encoder
            .hardware
            .or(self.decoder)
            .map_or(GpuUse::None, Hardware::gpu)
    }
}

/// How much of a job a family does (PGPU-02), best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Share {
    /// Decodes, filters and encodes: nothing on the CPU.
    Whole,
    /// Decodes and encodes; a filter runs on the CPU.
    DecodeAndEncode,
    /// Encodes what the CPU decoded.
    EncodeOnly,
}

/// The one GPU family `spec`'s video encode runs on (PGPU-02), and so its
/// encoder and decoder: of the families that encode the output codec,
/// the one that does the most of the job ([`Share`]), the first in
/// `transcode.hardware`'s order among equals (detection lists each
/// codec's encoders in that order). With none, software, decoding on the
/// first family that decodes the source (detection lists each sample's
/// decoders in that order). A family never decodes for another's
/// encoder: frames would go from one GPU to the other through memory.
///
/// The pick is of what detection found, not of this file's GPU decode
/// having failed before: the run that follows such a failure decodes in
/// software on the same encoder (`hardware_decode`). `None` for a video
/// that is not encoded, or a codec nothing here encodes.
pub fn placement<'a>(spec: &EncodeSpec<'a>) -> Option<Placement<'a>> {
    let Some(ConvertedVideo::Encode { codec, .. }) = spec.video else {
        return None;
    };
    let encoders = spec.encoders;
    let candidates = encoders.video.iter().filter(|e| e.codec == codec);
    let stream = spec.descriptor.video.as_ref();
    let decodes =
        |hardware: Hardware| stream.is_some_and(|s| decodes_source(encoders, s, hardware));
    let hardware = candidates
        .clone()
        .filter_map(|e| e.hardware.map(|h| (e, h)))
        .enumerate()
        .map(|(order, (encoder, family))| {
            let share = share_of(spec, family, decodes(family));
            ((share, order), encoder, family)
        })
        .min_by_key(|(rank, _, _)| *rank);
    let (encoder, decoder) = match hardware {
        Some((_, encoder, family)) => (encoder, decodes(family).then_some(family)),
        None => {
            let software = candidates.clone().find(|e| e.hardware.is_none())?;
            let source = stream.map(|s| codecs::video_codec(&s.codec));
            let first = encoders
                .hardware_decoders
                .iter()
                .filter(|d| Some(d.codec) == source)
                .map(|d| d.hardware)
                .find(|h| decodes(*h));
            (software, first)
        }
    };
    Some(Placement {
        encoder,
        decoder: decoder.filter(|_| spec.hardware_decode),
    })
}

/// How much of `spec`'s job `family` does, encoding it, and decoding it
/// when it `decodes` the source.
fn share_of(spec: &EncodeSpec<'_>, family: Hardware, decodes: bool) -> Share {
    match (decodes, decodes && whole_on(spec, family)) {
        (true, true) => Share::Whole,
        (true, false) => Share::DecodeAndEncode,
        (false, _) => Share::EncodeOnly,
    }
}

/// How much of `spec`'s video encode the GPU [`placement`] puts it on
/// does: `None` when it is encoded in software (or not at all). With one
/// card's detection in `spec`, how much that card would do: what the
/// cards are ranked by for a job (PGPU-05, `encode::pick`).
pub fn share(spec: &EncodeSpec<'_>) -> Option<Share> {
    let family = placement(spec)?.encoder.hardware?;
    let decodes = spec
        .descriptor
        .video
        .as_ref()
        .is_some_and(|stream| decodes_source(spec.encoders, stream, family));
    Some(share_of(spec, family, decodes))
}

/// The video encoder [`placement`] picks for `video`, an encode of
/// `descriptor`'s video with `subtitle`: what Server › Playback's plan
/// names, before or without a run.
pub fn planned_encoder(
    encoders: &Encoders,
    descriptor: &MediaDescriptor,
    video: ConvertedVideo,
    subtitle: Option<ConvertedSubtitle>,
    gpu_filters: bool,
) -> Option<VideoEncoder> {
    let spec = EncodeSpec {
        input: Path::new(""),
        sidecar: None,
        descriptor,
        video: Some(video),
        audio: None,
        subtitle,
        encoders,
        vaapi_device: Path::new(""),
        hardware_decode: true,
        gpu_filters,
        tone_map: ToneMapSetting::Auto,
        start: None,
        output: Output::Progressive {
            file: PathBuf::new(),
        },
    };
    placement(&spec).map(|place| place.encoder.clone())
}

/// Whether `hardware` decodes `stream`, as far as detection's tests and
/// what drivers take say. VA-API's tests were of 8-bit samples, and
/// [`gpu_decodes`] says what else drivers take. NVDEC's were of each
/// depth it is used for: a 10-bit source needs a 10-bit test decode to
/// have worked, and 10-bit H.264 has none. QSV's hwaccel is not set up.
fn decodes_source(encoders: &Encoders, stream: &VideoStream, hardware: Hardware) -> bool {
    let source = codecs::video_codec(&stream.codec);
    match hardware {
        Hardware::Vaapi => encoders.decodes(source, Hardware::Vaapi) && gpu_decodes(stream, source),
        Hardware::Nvenc => {
            encoders.decodes_depth(source, stream.bit_depth, Hardware::Nvenc)
                && four_two_zero(stream)
        }
        Hardware::Qsv => false,
    }
}

/// Whether `family`, decoding and encoding `spec`'s video, would run
/// every filter it needs on the GPU too: where [`cpu_filters`] leaves
/// none on the CPU. With GPU filters off, NVDEC's frames come back to
/// memory, so on NVIDIA only a job with no filter at all (no tone map,
/// no burn-in, no scale) is all on the GPU, as PGPU-02 had it.
fn whole_on(spec: &EncodeSpec<'_>, family: Hardware) -> bool {
    match family {
        Hardware::Vaapi | Hardware::Nvenc => cpu_filters(spec, family).is_empty(),
        Hardware::Qsv => false,
    }
}

/// Whether NVDEC's frames stay on the GPU for `spec`'s NVENC encode
/// (PGPU-03): GPU filters on, `scale_cuda` found (its test is of this very
/// path: a CUDA decode, filtered and encoded on the GPU), and every
/// filter the job needs runs on CUDA. A tone map or text burn-in takes
/// all of the job's filters to the CPU, rather than a chain half on each.
fn cuda_frames(spec: &EncodeSpec<'_>) -> bool {
    spec.gpu_filters && spec.encoders.cuda_scale && cpu_filters(spec, Hardware::Nvenc).is_empty()
}

/// A filter of a video encode that runs on the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuFilter {
    /// No tone map the job's GPU runs is chosen (`tone_mapper_on`):
    /// VA-API's driver has no `tonemap_vaapi` and libplacebo does not run,
    /// or GPU filters are off, or `transcode.tone_map` says the CPU.
    ToneMap,
    /// The driver has no `overlay_vaapi` or `overlay_cuda`, or GPU
    /// filters are off.
    Pictures,
    /// `overlay_cuda` and `overlay_vaapi` lay pictures over 8-bit video
    /// here, and the output keeps the source's 10 bits (HEVC, AV1).
    PicturesOverTenBits,
    /// Text is rendered by libass, on the CPU only. It is burned in only
    /// for a device that shows no text format at all, so it has no GPU
    /// path.
    Text,
    /// NVIDIA has no `scale_cuda`, or GPU filters are off. VA-API always
    /// scales on the GPU.
    Scale,
    /// libplacebo tone maps (PGPU-04): pictures are laid over its output
    /// on the CPU.
    PicturesAfterLibplacebo,
}

/// The filters of `spec`'s video encode that run on the CPU on `family`:
/// tone mapping and burn-in, unless GPU filters are on and the driver
/// runs them; and on NVIDIA the scale, unless GPU filters are on and
/// `scale_cuda` runs. VA-API always scales on the GPU.
fn cpu_filters(spec: &EncodeSpec<'_>, family: Hardware) -> Vec<CpuFilter> {
    let Some(ConvertedVideo::Encode {
        codec,
        height,
        tone_map,
        ..
    }) = spec.video
    else {
        return Vec::new();
    };
    let on = spec.gpu_filters;
    let encoders = spec.encoders;
    let stream = spec.descriptor.video.as_ref();
    // libplacebo tone maps and scales; what is burned in goes over its
    // output on the CPU (`libplacebo_graph`).
    let mapper = tone_mapper_on(spec, family);
    let placebo = tone_map && matches!(mapper, ToneMapper::Libplacebo(_));
    let (tonemaps, scales, overlays) = match family {
        Hardware::Vaapi => (
            mapper.on_gpu(),
            true,
            on && encoders.gpu_overlay && !placebo,
        ),
        Hardware::Nvenc => (
            placebo,
            (on && encoders.cuda_scale) || placebo,
            on && encoders.cuda_overlay && !placebo,
        ),
        Hardware::Qsv => (false, false, false),
    };
    let mut cpu = Vec::new();
    if tone_map && !tonemaps {
        cpu.push(CpuFilter::ToneMap);
    }
    if !scales && stream.is_some_and(|s| height < s.height) {
        cpu.push(CpuFilter::Scale);
    }
    // The GPU's overlays lay pictures over 8-bit video: NVIDIA's over
    // `yuv420p`, VA-API's as its chain has always fed it, `nv12`.
    let ten_bits_out = ten_bits_out(spec, codec, tone_map, Some(family));
    match burned(spec).ok().flatten().map(|track| track.kind) {
        Some(SubtitleKind::Bitmap) if placebo => cpu.push(CpuFilter::PicturesAfterLibplacebo),
        Some(SubtitleKind::Bitmap) if !overlays => cpu.push(CpuFilter::Pictures),
        Some(SubtitleKind::Bitmap) if ten_bits_out => {
            cpu.push(CpuFilter::PicturesOverTenBits);
        }
        Some(SubtitleKind::Bitmap) | None => {}
        Some(SubtitleKind::Text) => cpu.push(CpuFilter::Text),
    }
    cpu
}

/// Whether an encode of `spec`'s video to `codec` on `hardware`'s
/// encoder writes 10 bits (PGPU-06): HEVC or AV1, not tone mapped, of a
/// source deeper than 8 bits, on an encoder that took a 10-bit test frame
/// at detection. Anything else is written in 8 bits, as every encode was
/// on VA-API before.
pub(crate) fn ten_bits_out(
    spec: &EncodeSpec<'_>,
    codec: VideoCodec,
    tone_map: bool,
    hardware: Option<Hardware>,
) -> bool {
    codec != VideoCodec::H264
        && !tone_map
        && spec
            .descriptor
            .video
            .as_ref()
            .is_some_and(|s| s.bit_depth > 8)
        && spec
            .encoders
            .video
            .iter()
            .find(|e| e.codec == codec && e.hardware == hardware)
            .is_some_and(|e| spec.encoders.encodes_ten_bit(e))
}

/// The transfer an encode that keeps `stream`'s HDR writes (PGPU-06):
/// PQ for HDR10, HLG for HLG, as [`VideoStream::keepable_hdr`] says; `None`
/// for SDR, or HDR an encode cannot keep.
fn kept_transfer(stream: &VideoStream) -> Option<&'static str> {
    // Dolby Vision profile 5 is made HDR10 by libplacebo.
    if stream.needs_rpu() {
        return Some("smpte2084");
    }
    match stream.keepable_hdr()? {
        HdrFormat::Hlg => Some("arib-std-b67"),
        _ => Some("smpte2084"),
    }
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
/// CPU instead: the driver lacks one, NVIDIA does not tone map, or the
/// subtitle is text. Nothing when they are off, or the video is not
/// decoded and encoded on one GPU.
pub fn cpu_instead_of_gpu(spec: &EncodeSpec<'_>) -> Vec<&'static str> {
    let family = placement(spec).and_then(|place| {
        place
            .decoder
            .filter(|decoder| place.encoder.hardware == Some(*decoder))
    });
    let Some(family @ (Hardware::Vaapi | Hardware::Nvenc)) = family else {
        return Vec::new();
    };
    if !spec.gpu_filters {
        return Vec::new();
    }
    let vaapi = family == Hardware::Vaapi;
    cpu_filters(spec, family)
        .into_iter()
        // A forced tone map has a note of its own (`tone_map_note`).
        .filter(|filter| *filter != CpuFilter::ToneMap || spec.tone_map == ToneMapSetting::Auto)
        .map(|filter| match filter {
            CpuFilter::ToneMap if vaapi => {
                "the GPU tone maps neither with its driver (tonemap_vaapi) nor with libplacebo"
            }
            CpuFilter::ToneMap => "libplacebo does not tone map on the NVIDIA GPU",
            CpuFilter::Pictures if vaapi => {
                "the GPU's driver does not overlay pictures (overlay_vaapi)"
            }
            CpuFilter::Pictures => "the NVIDIA GPU does not overlay pictures (overlay_cuda)",
            CpuFilter::PicturesOverTenBits if vaapi => {
                "pictures are burned into 10-bit video on the CPU (overlay_vaapi is fed 8 bits)"
            }
            CpuFilter::PicturesOverTenBits => {
                "pictures are burned into 10-bit video on the CPU (overlay_cuda is 8-bit)"
            }
            CpuFilter::Text => "text subtitles are burned in on the CPU",
            CpuFilter::Scale => "the NVIDIA GPU does not scale (scale_cuda)",
            CpuFilter::PicturesAfterLibplacebo => {
                "pictures are burned in on the CPU after libplacebo's tone map"
            }
        })
        .collect()
}

/// What tone maps `spec`'s video when it runs on `family` (PGPU-04), if
/// it is tone mapped: as `transcode.tone_map` says, of what detection
/// found on that GPU. `"auto"` takes the GPU's own (`tonemap_vaapi`)
/// while `gpu_filters` is on, then libplacebo whatever `gpu_filters` says
/// (its tone map is the one a CPU spends minutes per film on, and it
/// matches the CPU's closely: the user's decision, 2026-10-10), then the
/// CPU; a forced method the GPU lacks is the CPU. A run after a GPU
/// failure (`hardware_decode` off) tone maps on the CPU.
fn tone_mapper_on(spec: &EncodeSpec<'_>, family: Hardware) -> ToneMapper {
    let encoders = spec.encoders;
    let own = (family == Hardware::Vaapi && encoders.gpu_tonemap).then_some(ToneMapper::Vaapi);
    let placebo = encoders
        .libplacebo(family)
        .map(ToneMapper::Libplacebo)
        .filter(|_| spec.hardware_decode);
    // Dolby Vision profile 5 is right only with its RPU applied, which
    // libplacebo does and the others do not (PGPU-06): libplacebo wherever
    // it runs, whatever the setting.
    if needs_rpu(spec)
        && let Some(placebo) = placebo
    {
        return placebo;
    }
    let chosen = match spec.tone_map {
        ToneMapSetting::Cpu => None,
        ToneMapSetting::Gpu => own,
        ToneMapSetting::Libplacebo => placebo,
        ToneMapSetting::Auto if !spec.gpu_filters => placebo,
        ToneMapSetting::Auto => own.or(placebo),
    };
    chosen.unwrap_or(ToneMapper::Cpu)
}

/// Whether `spec`'s video is tone mapped by libplacebo (PGPU-04), and on
/// which family's GPU, with which device: a tone-mapped video encoded on
/// a GPU whose chosen tone map is libplacebo. A software encode tone maps
/// on the CPU.
pub fn libplacebo(spec: &EncodeSpec<'_>) -> Option<(Hardware, VulkanDevice)> {
    // A tone map, or Dolby Vision profile 5 made HDR10 (PGPU-06).
    let Some(ConvertedVideo::Encode { tone_map, .. }) = spec.video else {
        return None;
    };
    if !tone_map && !needs_rpu(spec) {
        return None;
    }
    let family = placement(spec)?.encoder.hardware?;
    match tone_mapper_on(spec, family) {
        ToneMapper::Libplacebo(device) => Some((family, device)),
        ToneMapper::Cpu | ToneMapper::Vaapi => None,
    }
}

/// Whether `spec`'s source is Dolby Vision profile 5 (PGPU-06), whose
/// picture is right only with its RPU applied.
fn needs_rpu(spec: &EncodeSpec<'_>) -> bool {
    spec.descriptor
        .video
        .as_ref()
        .is_some_and(VideoStream::needs_rpu)
}

/// What tone mapped `spec`'s video, if it is tone mapped: libplacebo, VA-API
/// on frames kept on the GPU, or the CPU (PGPU-04).
pub fn tone_mapper(spec: &EncodeSpec<'_>) -> Option<ToneMapper> {
    let Some(ConvertedVideo::Encode { tone_map: true, .. }) = spec.video else {
        return None;
    };
    Some(match libplacebo(spec) {
        Some((_, device)) => ToneMapper::Libplacebo(device),
        None if decode(spec) == Decode::Gpu && gpu_decoder(spec) == Some(Hardware::Vaapi) => {
            ToneMapper::Vaapi
        }
        None => ToneMapper::Cpu,
    })
}

/// With a tone map forced by `transcode.tone_map`, why `spec`'s runs on
/// the CPU instead, if it does: what the server notes once (PGPU-04).
pub fn tone_map_note(spec: &EncodeSpec<'_>) -> Option<&'static str> {
    if tone_mapper(spec)? != ToneMapper::Cpu || !spec.hardware_decode {
        return None;
    }
    let family = placement(spec).and_then(|place| place.encoder.hardware);
    let chosen = family.map(|family| tone_mapper_on(spec, family));
    match (spec.tone_map, family, chosen) {
        (ToneMapSetting::Auto | ToneMapSetting::Cpu, _, _) => None,
        (_, None, _) => Some(
            "transcode.tone_map forces a GPU tone map, but the job is encoded in software; \
             tone mapped on the CPU",
        ),
        (ToneMapSetting::Gpu, _, Some(ToneMapper::Vaapi)) => Some(
            "transcode.tone_map is \"gpu\", but a filter of the job runs on the CPU, so its \
             tone map does too",
        ),
        (ToneMapSetting::Gpu, _, _) => Some(
            "transcode.tone_map is \"gpu\", but the job's GPU has no tone map of its own \
             (tonemap_vaapi); tone mapped on the CPU",
        ),
        (ToneMapSetting::Libplacebo, _, _) => Some(
            "transcode.tone_map is \"libplacebo\", but libplacebo does not run on the job's \
             GPU; tone mapped on the CPU",
        ),
    }
}

/// Whether NVIDIA's Vulkan device decodes `spec`'s source for a shared
/// libplacebo run: where NVDEC decodes it (detection's test), and only
/// H.264 and HEVC, the codecs Vulkan Video's decode began with. Others
/// decode on NVDEC into memory and are uploaded.
fn vulkan_decodes(spec: &EncodeSpec<'_>) -> bool {
    let codec = spec
        .descriptor
        .video
        .as_ref()
        .map(|s| codecs::video_codec(&s.codec));
    gpu_decoder(spec) == Some(Hardware::Nvenc)
        && matches!(codec, Some(VideoCodec::H264 | VideoCodec::Hevc))
}

/// Where a libplacebo run's video is decoded (PGPU-04). Shared: on the
/// GPU with the frames kept there for libplacebo, through Vulkan on
/// NVIDIA ([`vulkan_decodes`], else NVDEC into memory), through VA-API on
/// AMD; libplacebo's own device takes frames from memory, so the GPU
/// decodes into memory. With no GPU decode, in software.
fn libplacebo_decode(spec: &EncodeSpec<'_>, family: Hardware, device: VulkanDevice) -> Decode {
    if gpu_decoder(spec).is_none() {
        return Decode::Software;
    }
    match (family, device) {
        (Hardware::Nvenc, VulkanDevice::Shared) if !vulkan_decodes(spec) => Decode::Hardware,
        (_, VulkanDevice::Shared) => Decode::Gpu,
        (_, VulkanDevice::Own) => Decode::Hardware,
    }
}

/// The Vulkan a run uses (PGPU-04), for its jail: on the VA-API GPU's
/// render node, or on NVIDIA's GPU (chosen by name); none without
/// libplacebo.
pub fn vulkan(spec: &EncodeSpec<'_>) -> Option<Vulkan> {
    match libplacebo(spec)? {
        (Hardware::Nvenc, _) => Some(Vulkan::default()),
        _ => Some(Vulkan::on(spec.vaapi_device)),
    }
}

/// Whether `spec`'s video is made all on the GPU, nothing on the CPU but
/// moving frames: decoded and filtered on it and encoded from it, or tone
/// mapped by libplacebo on its own device from frames the GPU decoded
/// into memory, with nothing burned in. What weighs a quarter.
pub fn all_on_gpu(spec: &EncodeSpec<'_>) -> bool {
    match (decode(spec), libplacebo(spec)) {
        (Decode::Software, _) => false,
        // A burn-in after libplacebo is on the CPU, the frames on the GPU
        // or not.
        (_, Some((family, _))) => cpu_filters(spec, family).is_empty(),
        (decode, None) => decode == Decode::Gpu,
    }
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
    /// On NVIDIA's GPU through Vulkan, for libplacebo (PGPU-04).
    Vulkan,
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
    /// What tone mapped (PGPU-04), when the video is tone mapped.
    pub tone_mapper: Option<ToneMapper>,
    pub burn_in: FilterOn,
    /// The picture written, width × height: the source's, or scaled to
    /// the plan's height as `scale=-2:h` (or `scale_vaapi=w=-2`,
    /// `scale_cuda=w=-2`) scales it. `None` with no video.
    pub size: Option<(u32, u32)>,
    /// The encoder's stated target (`-b:v`), when it has one.
    pub target_bitrate: Option<u64>,
}

/// [`Made`] for `spec`: the same decisions [`ffmpeg_args`] makes.
pub fn made(spec: &EncodeSpec<'_>) -> Made {
    let Some(ConvertedVideo::Encode {
        max_bitrate,
        tone_map,
        ..
    }) = spec.video
    else {
        return Made {
            decoder: Decoder::None,
            encoder: None,
            tone_map: FilterOn::None,
            tone_mapper: None,
            burn_in: FilterOn::None,
            size: written_size(spec),
            target_bitrate: None,
        };
    };
    let decode = decode(spec);
    let placebo = libplacebo(spec);
    // Every filter is on the GPU or none is: `decode` keeps the frames
    // there only when nothing on the CPU needs them. But for libplacebo,
    // which tone maps on the GPU whatever burns in after it on the CPU.
    let on = |used: bool| match (used, decode) {
        (false, _) => FilterOn::None,
        (true, Decode::Gpu) if placebo.is_none() => FilterOn::Gpu,
        (true, _) => FilterOn::Cpu,
    };
    let tone_mapper = tone_mapper(spec);
    Made {
        decoder: match gpu_decoder(spec) {
            None => Decoder::Software,
            Some(Hardware::Nvenc) if placebo.is_some() && decode == Decode::Gpu => Decoder::Vulkan,
            Some(hardware) => Decoder::Hardware(hardware),
        },
        encoder: placement(spec).map(|place| place.encoder.clone()),
        tone_map: match tone_mapper {
            Some(mapper) if mapper.on_gpu() => FilterOn::Gpu,
            _ => on(tone_map),
        },
        tone_mapper,
        burn_in: on(matches!(burned(spec), Ok(Some(_)))),
        size: written_size(spec),
        target_bitrate: max_bitrate,
    }
}

/// The picture `spec` writes, width × height: a copy's is the source's;
/// an encode's is scaled to the plan's height as `scale=-2:h` does, or to
/// the size AV1 on VA-API can write ([`whole_av1_size`]), as the filter
/// graph scales it. What [`Made`] reports and the HLS playlist states
/// (`RESOLUTION`), so neither says a size the encode does not write.
/// `None` with no video.
pub fn written_size(spec: &EncodeSpec<'_>) -> Option<(u32, u32)> {
    let stream = spec.descriptor.video.as_ref()?;
    match spec.video? {
        ConvertedVideo::Encode { height, .. } => Some(
            whole_av1_size(spec, stream, height)
                .unwrap_or_else(|| scaled(stream.width, stream.height, height)),
        ),
        ConvertedVideo::Copy { .. } => Some((stream.width, stream.height)),
        ConvertedVideo::Unknown => None,
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

/// The picture an AV1 encode on VA-API writes, when the size `scaled`
/// gives is not one it can: a width of 64s and a height of 16s. AMD's
/// encoder (radeonsi) codes any other size padded out to them, and says
/// so in its sequence header but not as a smaller render size: 1296×540
/// is coded 1344×544, 1920×1080 1920×1082, while the track and its
/// `av1C` say the size asked for. AVPlayer configures its decoder from
/// `av1C` and gets a different sequence header in every key frame: on a
/// switch to such a rung it played the sound over a black or frozen
/// picture. Measured on nimble's 7900 XTX: every 64×16 size was coded
/// as asked. `None` for any other encoder, and for a size already whole.
///
/// The height is cut to the 16s at or under it, so a device's height
/// limit holds; the width is that height's, kept in proportion, to the
/// nearest 64 (at most 32 px off, 2.5 % at 1280 wide).
fn whole_av1_size(spec: &EncodeSpec<'_>, stream: &VideoStream, height: u32) -> Option<(u32, u32)> {
    let encoder = placement(spec)?.encoder;
    if encoder.codec != VideoCodec::Av1 || encoder.hardware != Some(Hardware::Vaapi) {
        return None;
    }
    let (width, to) = scaled(stream.width, stream.height, height);
    if width % 64 == 0 && to % 16 == 0 {
        return None;
    }
    let h = (to / 16 * 16).max(16);
    let natural = u64::from(stream.width) * u64::from(h) / u64::from(stream.height.max(1));
    let w = ((natural + 32) / 64 * 64).max(64) as u32;
    Some((w, h))
}

/// Whether VA-API drivers decode `stream` at all: 4:2:0, and H.264 only
/// in 8 bits. Detection's test decode is of 8-bit 4:2:0, so it says
/// nothing of the rest. One this lets through that the driver still
/// refuses, ffmpeg decodes in software itself, or the job is run again
/// in software (`job`).
fn gpu_decodes(stream: &VideoStream, codec: VideoCodec) -> bool {
    four_two_zero(stream) && !(codec == VideoCodec::H264 && stream.bit_depth > 8)
}

/// Whether `stream` is 4:2:0, all detection's samples are: the only
/// chroma every GPU decoder takes (NVDEC's HEVC 4:4:4 is Turing on).
fn four_two_zero(stream: &VideoStream) -> bool {
    stream.pixel_format.as_deref().is_none_or(|format| {
        ["yuv420p", "yuvj420p", "nv12", "p010"]
            .iter()
            .any(|ok| format.starts_with(ok))
    })
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
    ten_bits: bool,
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
            (_, SubtitleKind::Text) => chain.push(text_burn_in(spec, sub.index, picked)?),
        }
    }
    if tone_map {
        chain.push(s(TONE_MAP));
    }
    if height < stream.height {
        chain.push(format!("scale=-2:{height}"));
    }
    if vaapi && ten_bits {
        // HDR kept (PGPU-06): 10 bits to the encoder.
        chain.push(s("format=p010,hwupload"));
    } else if vaapi {
        chain.push(s("format=nv12,hwupload"));
    } else if codec == VideoCodec::H264 && !tone_map {
        // H.264 decoders expect 8-bit 4:2:0.
        chain.push(s("format=yuv420p"));
    } else if !ten_bits && !tone_map && stream.bit_depth > 8 {
        // An encoder that took no 10-bit test frame gets 8 bits.
        chain.push(s("format=yuv420p"));
    }
    // Pictures laid over a picture nothing else changes end the graph at
    // the overlay: `overlaynull` is no filter, and a 1080p HEVC encode of
    // a Blu-ray with PGS failed on it, on the GPU and in software alike.
    Ok(if source.ends_with("overlay") {
        if chain.is_empty() {
            format!("{source}[v]")
        } else {
            format!("{source},{}[v]", chain.join(","))
        }
    } else if chain.is_empty() {
        format!("{source}null[v]")
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
    ten_bits: bool,
) -> Result<String, String> {
    let mut chain = Vec::new();
    if tone_map {
        chain.push(s(TONEMAP_VAAPI));
    }
    let mut options = Vec::new();
    if height < stream.height {
        options.push(format!("w=-2:h={height}"));
    }
    // An encode that keeps HDR (PGPU-06) keeps the decode's 10 bits.
    if stream.bit_depth > 8 && !tone_map && !ten_bits {
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

/// The video's filters on NVIDIA's GPU (PGPU-03), for frames NVDEC left
/// there: `scale_cuda` scales to an even width as the CPU's `scale=-2`
/// does, and converts a 10-bit picture to 8 bits for H.264, as the CPU's
/// chain does with `format=yuv420p`; HEVC and AV1 keep the source's
/// depth, as on the CPU. Burned-in pictures are scaled on the CPU to the
/// picture written, uploaded to the decode's device (named by
/// [`cuda_device_args`]) and laid over it after the scale: the same
/// picture as the CPU's overlay-then-scale, with a smaller upload.
/// `overlay_cuda` lays `yuva420p` over `yuv420p` only, so the video is
/// converted to it; [`cpu_filters`] keeps a 10-bit output's pictures on
/// the CPU. [`decode`] chose this path, so CUDA runs every filter asked
/// for (no tone map: NVIDIA's is on the CPU).
fn cuda_graph(
    spec: &EncodeSpec<'_>,
    stream: &VideoStream,
    height: u32,
    ten_bits: bool,
) -> Result<String, String> {
    let burned = burned(spec)?;
    let mut options = Vec::new();
    if height < stream.height {
        options.push(format!("w=-2:h={height}"));
    }
    if burned.is_some() {
        options.push(s("format=yuv420p"));
    } else if stream.bit_depth > 8 && !ten_bits {
        // 8 bits for H.264, and for an encoder that took no 10-bit test
        // frame (PGPU-06).
        options.push(s("format=nv12"));
    }
    let video = format!("[0:{}]", stream.index);
    let scale = format!("scale_cuda={}", options.join(":"));
    Ok(match burned {
        Some(track) => {
            let (width, height) = scaled(stream.width, stream.height, height);
            format!(
                "[{}]scale={width}:{height},format=yuva420p,hwupload[subs];\
                 {video}{scale}[main];[main][subs]overlay_cuda[v]",
                pictures_input(&track.source),
            )
        }
        None if options.is_empty() => format!("{video}null[v]"),
        None => format!("{video}{scale}[v]"),
    })
}

/// The filter that burns text track `track` (the plan's subtitle `index`)
/// in: libass, reading the film or the external file itself.
fn text_burn_in(
    spec: &EncodeSpec<'_>,
    index: u32,
    track: &SubtitleStream,
) -> Result<String, String> {
    Ok(match &track.source {
        SubtitleSource::Embedded { .. } => {
            let nth = spec.descriptor.subtitles[..index as usize]
                .iter()
                .filter(|s| matches!(s.source, SubtitleSource::Embedded { .. }))
                .count();
            format!("subtitles=filename={}:si={nth}", filter_path(spec.input)?)
        }
        SubtitleSource::External { path } => {
            let path = spec.sidecar.unwrap_or(path);
            format!("subtitles=filename={}", filter_path(path)?)
        }
    })
}

/// The devices of a libplacebo run (PGPU-04), before the film's input,
/// and its decode, as [`libplacebo_decode`] chose it:
///
/// - **own** device, on VA-API: the encoder's VA-API device, which is the
///   filters' too (`hwupload`), libplacebo seeing no Vulkan device there
///   and making its own; the GPU's decode into memory.
/// - **shared**, on VA-API (experimental): a Vulkan device derived from
///   the VA-API one, the filters'; VA-API's frames kept, to be mapped.
/// - **shared**, on NVIDIA: ffmpeg's NVIDIA Vulkan device, the filters';
///   decoded on it (`-hwaccel vulkan`), or on NVDEC into memory.
fn libplacebo_devices(
    spec: &EncodeSpec<'_>,
    family: Hardware,
    device: VulkanDevice,
    decode: Decode,
) -> Vec<String> {
    let node = spec.vaapi_device.display();
    let mut args = Vec::new();
    match (family, device) {
        (Hardware::Nvenc, VulkanDevice::Shared) => {
            args.extend(["-init_hw_device", NVIDIA_VULKAN, "-filter_hw_device", "vk"].map(s));
            match decode {
                Decode::Gpu => args.extend(
                    [
                        "-hwaccel",
                        "vulkan",
                        "-hwaccel_device",
                        "vk",
                        "-hwaccel_output_format",
                        "vulkan",
                    ]
                    .map(s),
                ),
                Decode::Hardware => {
                    args.extend(hwaccel_args(Hardware::Nvenc, spec.vaapi_device, false));
                }
                Decode::Software => {}
            }
        }
        (_, VulkanDevice::Shared) => {
            args.extend([
                s("-init_hw_device"),
                format!("vaapi=va:{node}"),
                s("-init_hw_device"),
                s("vulkan=vk@va"),
                s("-filter_hw_device"),
                s("vk"),
            ]);
            if decode == Decode::Gpu {
                args.extend(
                    [
                        "-hwaccel",
                        "vaapi",
                        "-hwaccel_device",
                        "va",
                        "-hwaccel_output_format",
                        "vaapi",
                    ]
                    .map(s),
                );
            }
        }
        (_, VulkanDevice::Own) => {
            if family == Hardware::Vaapi {
                args.extend([s("-vaapi_device"), s(node)]);
            }
            if let (Some(decoder), Decode::Hardware) = (gpu_decoder(spec), decode) {
                args.extend(hwaccel_args(decoder, spec.vaapi_device, false));
            }
        }
    }
    args
}

/// The video's filters for a libplacebo run (PGPU-04): libplacebo tone
/// maps to 8-bit SDR BT.709 and scales to the plan's height
/// ([`libplacebo_filter`]); shared, on frames on ffmpeg's Vulkan device
/// (mapped there from VA-API, decoded there, or uploaded), downloaded
/// after it. A subtitle burned in goes over its output on the CPU, at the
/// size written: pictures scaled to it and laid over, text drawn by
/// libass. Then up to the VA-API encoder (its device found again from the
/// derived Vulkan one when shared), or to NVENC from memory.
fn libplacebo_graph(
    spec: &EncodeSpec<'_>,
    stream: &VideoStream,
    height: u32,
    (family, device): (Hardware, VulkanDevice),
    decode: Decode,
) -> Result<String, String> {
    let vaapi = family == Hardware::Vaapi;
    let mut head = Vec::new();
    match (device, decode) {
        (VulkanDevice::Shared, Decode::Gpu) if vaapi => head.push(s("hwmap=derive_device=vulkan")),
        (VulkanDevice::Shared, Decode::Gpu) | (VulkanDevice::Own, _) => {}
        (VulkanDevice::Shared, _) => head.push(s("hwupload")),
    }
    // Dolby Vision profile 5 not tone mapped is made HDR10, in 10 bits
    // (PGPU-06); anything else libplacebo runs on is tone mapped to 8.
    let hdr10 = matches!(
        spec.video,
        Some(ConvertedVideo::Encode {
            tone_map: false,
            ..
        })
    );
    let scale = (height < stream.height).then_some(height);
    head.push(if hdr10 {
        spoolframe_media::encoders::libplacebo_hdr10_filter(scale)
    } else {
        libplacebo_filter(scale)
    });
    let format = if hdr10 { "p010" } else { "nv12" };
    if device == VulkanDevice::Shared {
        head.push(format!("hwdownload,format={format}"));
    }
    let own_upload = format!("format={format},hwupload");
    let tail = match (vaapi, device) {
        (true, VulkanDevice::Shared) => Some("hwupload=derive_device=vaapi"),
        (true, VulkanDevice::Own) => Some(own_upload.as_str()),
        (false, _) => None,
    };
    let video = format!("[0:{}]", stream.index);
    let index = spec.subtitle.map_or(0, |sub| sub.index);
    Ok(match burned(spec)? {
        Some(track) if track.kind == SubtitleKind::Bitmap => {
            let (width, height) = scaled(stream.width, stream.height, height);
            let tail = tail.map_or_else(String::new, |tail| format!(",{tail}"));
            format!(
                "[{}]scale={width}:{height}[subs];{video}{}[main];[main][subs]overlay{tail}[v]",
                pictures_input(&track.source),
                head.join(",")
            )
        }
        burned => {
            if let Some(track) = burned {
                head.push(text_burn_in(spec, index, track)?);
            }
            head.extend(tail.map(s));
            format!("{video}{}[v]", head.join(","))
        }
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
                        format!("{graph},scale_vaapi=w={w}:h={h}[v]")
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
                        cuts: Some(cuts), ..
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

    // Audio.
    if let Some(audio) = spec.audio {
        let stream = d
            .audio
            .get(audio.index as usize)
            .ok_or_else(|| format!("audio {}: no such track", audio.index))?;
        args.extend([s("-map"), format!("0:{}", stream.index)]);
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
        Output::Hls {
            dir,
            start_number,
            cuts,
        } => {
            // A copied video is cut at every keyframe (see `hls`): a length
            // under any GOP does that. So is a remux ladder's encoded rung,
            // whose keyframes are the copy's cuts and no others.
            let cut = match spec.video {
                Some(ConvertedVideo::Copy { .. }) => s("0.1"),
                _ if cuts.is_some() => s("0.1"),
                _ => s(SEGMENT_SECS),
            };
            // An encoded video's run from 0 cuts its short head (PX-26):
            // the muxer's thresholds are `hls_init_time` × n until its list
            // is full, then go on by `hls_time` from the list's end. With
            // one entry that is 2 s, 4 s, 8 s, then every 6 s: the grid of
            // `hls::Segments::encoded`. The list is ffmpeg's own playlist,
            // which the server reads only for its last segment (`job`).
            let list_size = match spec.video {
                Some(ConvertedVideo::Encode { .. }) if from_start(spec) && cuts.is_none() => {
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
                dolby_vision: None,
                bit_rate: None,
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
            cuda_scale: false,
            cuda_overlay: false,
            libplacebo: Vec::new(),
            ten_bit: Vec::new(),
            unusable: Vec::new(),
            failed: Vec::new(),
        }
    }

    #[test]
    fn video_planned_over_a_file_without_any_is_refused() {
        let mut film = film();
        film.video = None;
        let enc = encoders(false);
        let spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, hls());
        assert_eq!(ffmpeg_args(&spec).unwrap_err(), "video: no such stream");
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
            tone_map: ToneMapSetting::Auto,
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
                    bit_depth: 8,
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
                bit_depth: 8,
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

    /// NVENC for H.264 and HEVC, and NVDEC's decoders as a GTX 1060's
    /// tests find them: H.264, 8- and 10-bit HEVC, VP9; no AV1.
    fn nvidia() -> Encoders {
        let nvenc = |codec, name: &str| VideoEncoder {
            codec,
            name: name.into(),
            hardware: Some(Hardware::Nvenc),
        };
        let nvdec = |codec, bit_depth| HardwareDecoder {
            codec,
            hardware: Hardware::Nvenc,
            bit_depth,
        };
        Encoders {
            video: vec![
                nvenc(VideoCodec::H264, "h264_nvenc"),
                nvenc(VideoCodec::Hevc, "hevc_nvenc"),
            ],
            hardware_decoders: vec![
                nvdec(VideoCodec::H264, 8),
                nvdec(VideoCodec::Hevc, 8),
                nvdec(VideoCodec::Hevc, 10),
                nvdec(VideoCodec::Vp9, 8),
            ],
            ..encoders(false)
        }
    }

    /// PGPU-01: the tester's case, 4K HDR10 HEVC to 1080p H.264 on NVENC,
    /// here with PGS burned in too. NVDEC decodes (`-hwaccel cuda`, an
    /// input option before the seek), the frames come back to memory, and
    /// the burn-in, tone map and scale run on the CPU as after a software
    /// decode; NVENC takes the frames from memory. GPU filters on, on a
    /// GPU where no CUDA filter was found, change nothing but the reasons
    /// logged.
    #[test]
    fn an_nvenc_encode_decodes_on_nvdec_with_a_cpu_tone_map_and_burn_in() {
        let (film, enc) = (film(), nvidia());
        let mut spec = spec_with(
            &film,
            &enc,
            TONE_MAPPED_H264,
            burn_in(0, SubtitleFormat::Pgs),
        );
        spec.start = Some(Duration::from_secs(600));
        spec.gpu_filters = true;
        assert_eq!(decode(&spec), Decode::Hardware);
        assert_eq!(gpu_decoder(&spec), Some(Hardware::Nvenc));
        let args = ffmpeg_args(&spec).unwrap();
        assert_eq!(
            args[..11],
            [
                "-hide_banner",
                "-nostdin",
                "-y",
                "-loglevel",
                "error",
                "-hwaccel",
                "cuda",
                "-ss",
                "600.000",
                "-i",
                "/media/Film.mkv"
            ]
        );
        for vaapi_only in [
            "-hwaccel_output_format",
            "-hwaccel_device",
            "-vaapi_device",
            "-init_hw_device",
        ] {
            assert!(!args.contains(&s(vaapi_only)), "{vaapi_only}: {args:?}");
        }
        assert_eq!(
            after(&args, "-filter_complex"),
            format!("[0:2]scale=3840:2160[subs];[0:0][subs]overlay,{TONE_MAP},scale=-2:1080[v]")
        );
        assert!(has(&args, &["-c:v", "h264_nvenc"]));
        assert_eq!(
            cpu_instead_of_gpu(&spec),
            [
                "libplacebo does not tone map on the NVIDIA GPU",
                "the NVIDIA GPU does not scale (scale_cuda)",
                "the NVIDIA GPU does not overlay pictures (overlay_cuda)"
            ]
        );
        // PX-24 says so.
        let made = made(&spec);
        assert_eq!(made.decoder, Decoder::Hardware(Hardware::Nvenc));
        assert_eq!(made.encoder.map(|e| e.name), Some(s("h264_nvenc")));
        assert_eq!(
            (made.tone_map, made.burn_in),
            (FilterOn::Cpu, FilterOn::Cpu)
        );
        assert_eq!(made.size, Some((1920, 1080)));
    }

    /// The same graph as a software decode's: NVDEC only moves the decode.
    #[test]
    fn an_nvdec_decode_filters_as_a_software_one_does() {
        let (film, enc) = (film(), nvidia());
        let on_gpu = spec_with(
            &film,
            &enc,
            TONE_MAPPED_H264,
            burn_in(0, SubtitleFormat::Pgs),
        );
        let software = EncodeSpec {
            hardware_decode: false,
            ..on_gpu.clone()
        };
        let (gpu_args, cpu_args) = (
            ffmpeg_args(&on_gpu).unwrap(),
            ffmpeg_args(&software).unwrap(),
        );
        let without_hwaccel: Vec<&String> = gpu_args
            .iter()
            .enumerate()
            .filter(|(i, a)| *a != "-hwaccel" && !(*i > 0 && gpu_args[i - 1] == "-hwaccel"))
            .map(|(_, a)| a)
            .collect();
        assert_eq!(without_hwaccel, cpu_args.iter().collect::<Vec<_>>());
    }

    /// What NVDEC's tests did not decode, or a stream they did not cover,
    /// decodes in software: AV1 on Pascal, 10-bit H.264 (no NVIDIA GPU
    /// decodes it), 4:2:2, 10-bit HEVC where only 8-bit was decoded; and
    /// VA-API's decoders are not NVENC's.
    #[test]
    fn a_source_nvdec_lacks_decodes_in_software() {
        let enc = nvidia();
        let with = |change: fn(&mut VideoStream)| {
            let mut film = film();
            change(film.video.as_mut().unwrap());
            film
        };
        let av1 = with(|v| v.codec = "av1".into());
        let high10 = with(|v| {
            v.codec = "h264".into();
            v.profile = Some("High 10".into());
        });
        let four_two_two = with(|v| v.pixel_format = Some("yuv422p10le".into()));
        for (name, film) in [("av1", &av1), ("high10", &high10), ("4:2:2", &four_two_two)] {
            let spec = spec_with(film, &enc, TONE_MAPPED_H264, None);
            assert_eq!(decode(&spec), Decode::Software, "{name}");
            assert!(
                !ffmpeg_args(&spec).unwrap().contains(&s("-hwaccel")),
                "{name}"
            );
            assert_eq!(made(&spec).decoder, Decoder::Software, "{name}");
        }
        // 10-bit HEVC where only 8-bit decoded.
        let film = film();
        let eight_bit = Encoders {
            hardware_decoders: enc
                .hardware_decoders
                .iter()
                .filter(|d| d.bit_depth == 8)
                .cloned()
                .collect(),
            ..nvidia()
        };
        let spec = spec_with(&film, &eight_bit, TONE_MAPPED_H264, None);
        assert_eq!(decode(&spec), Decode::Software);
        // VA-API's decoders, beside an NVENC encode.
        let vaapi_decoders = Encoders {
            hardware_decoders: with_decoders(true).hardware_decoders,
            ..nvidia()
        };
        let spec = spec_with(&film, &vaapi_decoders, plain_h264(1080), None);
        assert_eq!(decode(&spec), Decode::Software);
        assert!(!ffmpeg_args(&spec).unwrap().contains(&s("-hwaccel")));
    }

    /// QSV is as it was: its hwaccel is not set up, so it decodes in
    /// software whatever was found.
    #[test]
    fn a_qsv_encode_decodes_in_software() {
        let film = film();
        let mut enc = nvidia();
        for encoder in &mut enc.video {
            encoder.hardware = Some(Hardware::Qsv);
            encoder.name = encoder.name.replace("nvenc", "qsv");
        }
        for decoder in &mut enc.hardware_decoders {
            decoder.hardware = Hardware::Qsv;
        }
        let spec = spec_with(&film, &enc, plain_h264(1080), None);
        assert_eq!(decode(&spec), Decode::Software);
        assert!(!ffmpeg_args(&spec).unwrap().contains(&s("-hwaccel")));
    }

    /// A film whose NVDEC decode failed before decodes in software, still
    /// encoded on NVENC: the run the job falls back to.
    #[test]
    fn an_nvenc_encode_whose_gpu_decode_failed_decodes_in_software() {
        let (film, enc) = (film(), nvidia());
        let mut spec = spec_with(&film, &enc, TONE_MAPPED_H264, None);
        spec.hardware_decode = false;
        assert_eq!(decode(&spec), Decode::Software);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(!args.contains(&s("-hwaccel")), "{args:?}");
        assert!(has(&args, &["-c:v", "h264_nvenc"]));
        assert_eq!(made(&spec).decoder, Decoder::Software);
    }

    // PGPU-06: an encode that keeps HDR writes 10 bits.

    /// VA-API's encoders, HEVC's having taken a 10-bit test frame, and
    /// its decoder for 10-bit HEVC where `decodes`.
    fn ten_bit_vaapi(decodes: bool) -> Encoders {
        let mut enc = encoders(true);
        enc.ten_bit = enc
            .video
            .iter()
            .filter(|e| e.codec == VideoCodec::Hevc)
            .cloned()
            .collect();
        if decodes {
            enc.hardware_decoders = vec![HardwareDecoder {
                codec: VideoCodec::Hevc,
                hardware: Hardware::Vaapi,
                bit_depth: 10,
            }];
        }
        enc
    }

    const HEVC_KEPT: ConvertedVideo = ConvertedVideo::Encode {
        codec: VideoCodec::Hevc,
        height: 1080,
        max_bitrate: None,
        tone_map: false,
    };

    /// HDR10 HEVC decoded on the GPU and kept: scaled in 10 bits on it,
    /// encoded Main 10, the HDR colours said.
    #[test]
    fn a_kept_hdr_encode_stays_ten_bit_on_the_vaapi_gpu() {
        let (film, enc) = (film(), ten_bit_vaapi(true));
        let spec = spec_with(&film, &enc, HEVC_KEPT, None);
        assert_eq!(decode(&spec), Decode::Gpu);
        let args = ffmpeg_args(&spec).unwrap();
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale_vaapi=w=-2:h=1080[v]"
        );
        assert!(has(&args, &["-c:v", "hevc_vaapi", "-profile:v", "main10"]));
        assert!(has(
            &args,
            &[
                "-color_primaries",
                "bt2020",
                "-color_trc",
                "smpte2084",
                "-colorspace",
                "bt2020nc"
            ]
        ));
    }

    /// The same decoded in memory: uploaded as `p010`, not `nv12`.
    #[test]
    fn a_kept_hdr_encode_uploads_ten_bits() {
        let (film, enc) = (film(), ten_bit_vaapi(false));
        let args = ffmpeg_args(&spec_with(&film, &enc, HEVC_KEPT, None)).unwrap();
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale=-2:1080,format=p010,hwupload[v]"
        );
        assert!(has(&args, &["-profile:v", "main10"]));
    }

    /// An encoder that took no 10-bit test frame gets 8 bits, and a tone
    /// map writes 8 bits whatever the encoder: no Main 10, no HDR colours.
    #[test]
    fn eight_bits_without_a_ten_bit_encoder_or_with_a_tone_map() {
        let film = film();
        let mut enc = ten_bit_vaapi(true);
        enc.ten_bit.clear();
        let args = ffmpeg_args(&spec_with(&film, &enc, HEVC_KEPT, None)).unwrap();
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale_vaapi=w=-2:h=1080:format=nv12[v]"
        );
        assert!(!args.iter().any(|a| a == "main10" || a == "-color_trc"));
        let enc = ten_bit_vaapi(false);
        let tone_mapped = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: None,
            tone_map: true,
        };
        let args = ffmpeg_args(&spec_with(&film, &enc, tone_mapped, None)).unwrap();
        let graph = after(&args, "-filter_complex");
        assert!(graph.ends_with("format=nv12,hwupload[v]"), "{graph}");
        assert!(!args.iter().any(|a| a == "main10" || a == "smpte2084"));
    }

    // PGPU-03: NVIDIA's filters on the GPU, `scale_cuda` and `overlay_cuda`.

    /// `nvidia()` where detection found these CUDA filters.
    fn cuda(scale: bool, overlay: bool) -> Encoders {
        Encoders {
            cuda_scale: scale,
            cuda_overlay: overlay,
            ..nvidia()
        }
    }

    /// `film()` as SDR 4K HEVC of `bit_depth` bits.
    fn sdr_4k(bit_depth: u32) -> MediaDescriptor {
        let mut film = film();
        let video = film.video.as_mut().unwrap();
        video.bit_depth = bit_depth;
        video.pixel_format = Some(
            if bit_depth > 8 {
                "yuv420p10le"
            } else {
                "yuv420p"
            }
            .into(),
        );
        video.color_transfer = None;
        video.hdr = None;
        film
    }

    /// `spec_with`, GPU filters on.
    fn filtered<'a>(
        film: &'a MediaDescriptor,
        encoders: &'a Encoders,
        video: ConvertedVideo,
        subtitle: Option<ConvertedSubtitle>,
    ) -> EncodeSpec<'a> {
        EncodeSpec {
            gpu_filters: true,
            ..spec_with(film, encoders, video, subtitle)
        }
    }

    /// SDR 4K to 1080p H.264 on NVENC with GPU filters on: NVDEC's
    /// frames stay on the GPU (an input option, before the seek) and
    /// `scale_cuda` scales them; nothing is uploaded or filtered on the
    /// CPU, and PX-24 and the weight say all on the GPU.
    #[test]
    fn an_nvenc_encode_scales_on_cuda_with_gpu_filters_on() {
        let (film, enc) = (sdr_4k(8), cuda(true, true));
        let mut spec = filtered(&film, &enc, plain_h264(1080), None);
        spec.start = Some(Duration::from_secs(600));
        assert_eq!(decode(&spec), Decode::Gpu);
        assert_eq!(gpu_decoder(&spec), Some(Hardware::Nvenc));
        let args = ffmpeg_args(&spec).unwrap();
        assert_eq!(
            args[5..13],
            [
                "-hwaccel",
                "cuda",
                "-hwaccel_output_format",
                "cuda",
                "-ss",
                "600.000",
                "-i",
                "/media/Film.mkv"
            ]
        );
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale_cuda=w=-2:h=1080[v]"
        );
        assert!(has(&args, &["-map", "[v]", "-c:v", "h264_nvenc"]));
        for absent in ["-vaapi_device", "-init_hw_device", "-hwaccel_device"] {
            assert!(!args.contains(&s(absent)), "{absent}: {args:?}");
        }
        assert!(cpu_instead_of_gpu(&spec).is_empty());
        let made = made(&spec);
        assert_eq!(made.decoder, Decoder::Hardware(Hardware::Nvenc));
        assert_eq!(made.size, Some((1920, 1080)));
        assert_eq!(
            (made.tone_map, made.burn_in),
            (FilterOn::None, FilterOn::None)
        );
        // At its own height, 8-bit to H.264: nothing to filter.
        let same = filtered(&film, &enc, plain_h264(2160), None);
        assert_eq!(decode(&same), Decode::Gpu);
        assert_eq!(
            after(&ffmpeg_args(&same).unwrap(), "-filter_complex"),
            "[0:0]null[v]"
        );
    }

    /// Pictures burned in on CUDA: scaled on the CPU to the picture
    /// written, uploaded to the decode's own device (named, one for
    /// both), and laid over the scaled video, converted to `yuv420p` for
    /// `overlay_cuda`. Embedded PGS, then an external file.
    #[test]
    fn an_nvenc_encode_burns_pictures_in_on_cuda() {
        let (film, enc) = (sdr_4k(8), cuda(true, true));
        for (index, pictures) in [(0, "0:2"), (3, "1:0")] {
            let mut spec = filtered(
                &film,
                &enc,
                plain_h264(1080),
                burn_in(index, SubtitleFormat::Pgs),
            );
            spec.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
            assert_eq!(decode(&spec), Decode::Gpu, "subtitle {index}");
            let args = ffmpeg_args(&spec).unwrap();
            assert!(
                has(
                    &args,
                    &[
                        "-init_hw_device",
                        "cuda=gpu",
                        "-filter_hw_device",
                        "gpu",
                        "-hwaccel",
                        "cuda",
                        "-hwaccel_device",
                        "gpu",
                        "-hwaccel_output_format",
                        "cuda",
                        "-i",
                        "/media/Film.mkv"
                    ]
                ),
                "{args:?}"
            );
            assert_eq!(
                after(&args, "-filter_complex"),
                format!(
                    "[{pictures}]scale=1920:1080,format=yuva420p,hwupload[subs];\
                     [0:0]scale_cuda=w=-2:h=1080:format=yuv420p[main];\
                     [main][subs]overlay_cuda[v]"
                )
            );
            assert!(cpu_instead_of_gpu(&spec).is_empty());
            let made = made(&spec);
            assert_eq!(made.burn_in, FilterOn::Gpu, "subtitle {index}");
            assert_eq!(made.tone_map, FilterOn::None);
        }
    }

    /// A 10-bit SDR source: to H.264, `scale_cuda` converts it to 8 bits
    /// as the CPU's `format=yuv420p` does; to HEVC it keeps its 10 bits,
    /// as on the CPU, and so pictures over it are burned in on the CPU
    /// (`overlay_cuda` is 8-bit).
    #[test]
    fn a_10_bit_sdr_source_is_made_8_bit_on_cuda_for_h264() {
        let (film, enc) = (sdr_4k(10), cuda(true, true));
        let spec = filtered(&film, &enc, plain_h264(1080), None);
        assert_eq!(decode(&spec), Decode::Gpu);
        assert_eq!(
            after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"),
            "[0:0]scale_cuda=w=-2:h=1080:format=nv12[v]"
        );
        let same = filtered(&film, &enc, plain_h264(2160), None);
        assert_eq!(
            after(&ffmpeg_args(&same).unwrap(), "-filter_complex"),
            "[0:0]scale_cuda=format=nv12[v]"
        );
        let hevc = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        };
        // HEVC keeps the 10 bits where its encoder took a 10-bit test
        // frame (PGPU-06), and is made 8-bit where it did not.
        let spec = filtered(&film, &enc, hevc, None);
        assert_eq!(
            after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"),
            "[0:0]scale_cuda=w=-2:h=1080:format=nv12[v]"
        );
        let mut enc = enc;
        enc.ten_bit = enc
            .video
            .iter()
            .filter(|e| e.name == "hevc_nvenc")
            .cloned()
            .collect();
        let spec = filtered(&film, &enc, hevc, None);
        assert_eq!(decode(&spec), Decode::Gpu);
        let args = ffmpeg_args(&spec).unwrap();
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale_cuda=w=-2:h=1080[v]"
        );
        assert!(has(&args, &["-c:v", "hevc_nvenc", "-profile:v", "main10"]));
        // SDR: no HDR colours to say.
        assert!(!args.iter().any(|a| a == "-color_trc"));
        let pictures = filtered(&film, &enc, hevc, burn_in(0, SubtitleFormat::Pgs));
        assert_eq!(decode(&pictures), Decode::Hardware);
        let graph = s(after(&ffmpeg_args(&pictures).unwrap(), "-filter_complex"));
        assert!(!graph.contains("_cuda"), "{graph}");
        assert_eq!(
            cpu_instead_of_gpu(&pictures),
            ["pictures are burned into 10-bit video on the CPU (overlay_cuda is 8-bit)"]
        );
    }

    /// An HDR source NVIDIA tone maps on the CPU: the frames come back to
    /// memory and every filter of the job runs there, as before PGPU-03,
    /// pictures too: no chain half on each.
    #[test]
    fn an_hdr_source_on_nvenc_filters_all_on_the_cpu() {
        let (film, enc) = (film(), cuda(true, true));
        let spec = filtered(
            &film,
            &enc,
            TONE_MAPPED_H264,
            burn_in(0, SubtitleFormat::Pgs),
        );
        assert_eq!(decode(&spec), Decode::Hardware);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(has(&args, &["-hwaccel", "cuda", "-i"]), "{args:?}");
        assert!(!args.contains(&s("-hwaccel_output_format")), "{args:?}");
        assert_eq!(
            after(&args, "-filter_complex"),
            format!("[0:2]scale=3840:2160[subs];[0:0][subs]overlay,{TONE_MAP},scale=-2:1080[v]")
        );
        assert_eq!(
            cpu_instead_of_gpu(&spec),
            ["libplacebo does not tone map on the NVIDIA GPU"]
        );
        let made = made(&spec);
        assert_eq!(
            (made.tone_map, made.burn_in),
            (FilterOn::Cpu, FilterOn::Cpu)
        );
    }

    /// GPU filters off: NVIDIA filters on the CPU, though CUDA could, and
    /// says nothing of it.
    #[test]
    fn with_gpu_filters_off_nvenc_scales_on_the_cpu() {
        let (film, enc) = (sdr_4k(8), cuda(true, true));
        let spec = spec_with(&film, &enc, plain_h264(1080), None);
        assert_eq!(decode(&spec), Decode::Hardware);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(!args.contains(&s("-hwaccel_output_format")), "{args:?}");
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:0]scale=-2:1080,format=yuv420p[v]"
        );
        assert!(cpu_instead_of_gpu(&spec).is_empty());
    }

    /// CUDA filters detection did not find run on the CPU, logged: no
    /// `scale_cuda`, no frames on the GPU at all; no `overlay_cuda`,
    /// pictures (and so the scale) on the CPU.
    #[test]
    fn cuda_filters_not_found_run_on_the_cpu() {
        let film = sdr_4k(8);
        let none = cuda(false, false);
        let spec = filtered(&film, &none, plain_h264(1080), None);
        assert_eq!(decode(&spec), Decode::Hardware);
        assert_eq!(
            after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"),
            "[0:0]scale=-2:1080,format=yuv420p[v]"
        );
        assert_eq!(
            cpu_instead_of_gpu(&spec),
            ["the NVIDIA GPU does not scale (scale_cuda)"]
        );
        // Nothing to scale: nothing on the CPU, and still nothing kept on
        // the GPU without `scale_cuda`'s test having passed.
        let same = filtered(&film, &none, plain_h264(2160), None);
        assert_eq!(decode(&same), Decode::Hardware);
        assert!(cpu_instead_of_gpu(&same).is_empty());
        let scale_only = cuda(true, false);
        let pgs = filtered(
            &film,
            &scale_only,
            plain_h264(1080),
            burn_in(0, SubtitleFormat::Pgs),
        );
        assert_eq!(decode(&pgs), Decode::Hardware);
        let graph = s(after(&ffmpeg_args(&pgs).unwrap(), "-filter_complex"));
        assert!(graph.contains("overlay,scale=-2:1080"), "{graph}");
        assert_eq!(
            cpu_instead_of_gpu(&pgs),
            ["the NVIDIA GPU does not overlay pictures (overlay_cuda)"]
        );
        // Text is libass's, on the CPU whatever the GPU has.
        let all = cuda(true, true);
        let text = filtered(
            &film,
            &all,
            plain_h264(1080),
            burn_in(1, SubtitleFormat::Srt),
        );
        assert_eq!(decode(&text), Decode::Hardware);
        assert_eq!(
            cpu_instead_of_gpu(&text),
            ["text subtitles are burned in on the CPU"]
        );
    }

    /// The CUDA path's fallback is the CPU's chain decoded in software,
    /// still on NVENC: what the job runs when a CUDA filter fails.
    #[test]
    fn the_cuda_path_s_fallback_is_the_cpu_chain() {
        let (film, enc) = (sdr_4k(8), cuda(true, true));
        let mut spec = filtered(
            &film,
            &enc,
            plain_h264(1080),
            burn_in(0, SubtitleFormat::Pgs),
        );
        assert_eq!(decode(&spec), Decode::Gpu);
        spec.hardware_decode = false;
        assert_eq!(decode(&spec), Decode::Software);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(!args.iter().any(|a| a.contains("cuda")), "{args:?}");
        assert_eq!(
            after(&args, "-filter_complex"),
            "[0:2]scale=3840:2160[subs];[0:0][subs]overlay,scale=-2:1080,format=yuv420p[v]"
        );
        assert!(has(&args, &["-c:v", "h264_nvenc"]));
        assert_eq!(made(&spec).burn_in, FilterOn::Cpu);
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

    /// Pictures burned into a 1080p SDR HEVC encode at its own size: the
    /// overlay is the whole graph, with nothing after it (no `overlaynull`,
    /// which is no filter). Review 2026-10-11 H1.
    #[test]
    fn pictures_over_an_unchanged_picture_end_the_graph_at_the_overlay() {
        let (film, enc) = (sdr_1080p(), encoders(false));
        let hevc = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        };
        let spec = spec_with(&film, &enc, hevc, burn_in(0, SubtitleFormat::Pgs));
        let args = ffmpeg_args(&spec).unwrap();
        let graph = after(&args, "-filter_complex");
        assert!(!graph.contains("overlaynull"), "{graph}");
        assert!(graph.ends_with("overlay[v]"), "{graph}");
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
            ["the GPU tone maps neither with its driver (tonemap_vaapi) nor with libplacebo"]
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

    /// NVENC with no NVDEC decoder found decodes in software, and filters
    /// on the CPU.
    #[test]
    fn made_says_nvenc_without_nvdec_decodes_in_software() {
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
                tone_mapper: None,
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
        dolby_vision: false,
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
            cuts: None,
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
            cuts: None,
        }
    }

    /// `film()` as Dolby Vision `profile` (HDR10 base, but profile 5).
    fn dv_film(profile: u8) -> MediaDescriptor {
        let mut film = film();
        let video = film.video.as_mut().unwrap();
        video.hdr = Some(HdrFormat::DolbyVision);
        video.dolby_vision = Some(spoolframe_media::DolbyVision {
            profile,
            level: 6,
            compatibility: if profile == 5 { 0 } else { 1 },
        });
        film
    }

    /// PGPU-06: a Dolby Vision copy labelled so is muxed with its record
    /// (`-strict unofficial`): profile 8 tagged `hvc1`, profile 5 `dvh1`.
    /// Copied as its base layer, it is muxed as any HEVC.
    #[test]
    fn a_labelled_dolby_vision_copy_keeps_its_record_and_tag() {
        let copy = |dolby_vision| ConvertedVideo::Copy {
            codec: VideoCodec::Hevc,
            dolby_vision,
        };
        let args_for = |film: &MediaDescriptor, video| {
            ffmpeg_args(&spec(film, &encoders(false), video, COPY_DTS, None, hls())).unwrap()
        };
        let p8 = dv_film(8);
        let args = args_for(&p8, copy(true));
        assert!(has(&args, &["-strict", "unofficial"]), "{args:?}");
        assert!(has(&args, &["-tag:v", "hvc1"]));
        let args = args_for(&p8, copy(false));
        assert!(!args.iter().any(|a| a == "unofficial"), "{args:?}");
        assert!(has(&args, &["-tag:v", "hvc1"]));
        let p5 = dv_film(5);
        let args = args_for(&p5, copy(true));
        assert!(has(&args, &["-tag:v", "dvh1"]), "{args:?}");
        assert!(has(&args, &["-strict", "unofficial"]));
    }

    /// Profile 7's enhancement layer is left out of a copy.
    #[test]
    fn a_profile_7_copy_drops_its_enhancement_layer() {
        let p7 = dv_film(7);
        let copy = ConvertedVideo::Copy {
            codec: VideoCodec::Hevc,
            dolby_vision: false,
        };
        let args = ffmpeg_args(&spec(&p7, &encoders(false), copy, COPY_DTS, None, hls())).unwrap();
        assert!(
            has(
                &args,
                &["-bsf:v", "hevc_mp4toannexb,filter_units=remove_types=63"]
            ),
            "{args:?}"
        );
        let args = ffmpeg_args(&spec(
            &film(),
            &encoders(false),
            copy,
            COPY_DTS,
            None,
            hls(),
        ))
        .unwrap();
        assert!(has(&args, &["-bsf:v", "hevc_mp4toannexb"]));
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
        // 8 bits: the encoder here took no 10-bit test frame (PGPU-06).
        assert_eq!(
            graph,
            "[0:0]subtitles=filename='/media/Film.mkv':si=1,format=yuv420p[v]"
        );
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

    /// AMD's AV1 encoder on VA-API codes a size it cannot write padded to
    /// 64×16, and the track's `av1C` says one size while every key frame
    /// says another. A rung of a 2.4:1 film at 540 (1296×540) is scaled to
    /// a whole one at the graph's end, on the GPU; one already whole, and
    /// any other encoder, keep `w=-2`.
    #[test]
    fn an_av1_vaapi_encode_writes_a_size_of_64_by_16() {
        use spoolframe_media::encoders::VideoEncoder;
        let mut film = film();
        if let Some(v) = film.video.as_mut() {
            v.codec = "h264".into();
            v.profile = Some("High".into());
            v.width = 1920;
            v.height = 800;
            v.pixel_format = Some("yuv420p".into());
            v.color_transfer = None;
            v.bit_depth = 8;
            v.hdr = None;
        }
        let mut enc = with_decoders(true);
        enc.video.push(VideoEncoder {
            codec: VideoCodec::Av1,
            name: "av1_vaapi".into(),
            hardware: Some(Hardware::Vaapi),
        });
        let run = |codec, height| {
            let output = Output::Hls {
                dir: "/scratch/h/0".into(),
                start_number: 1,
                cuts: None,
            };
            let video = ConvertedVideo::Encode {
                codec,
                height,
                max_bitrate: Some(1_000_000),
                tone_map: false,
            };
            let spec = spec(&film, &enc, video, COPY_DTS, None, output);
            (
                after(&ffmpeg_args(&spec).unwrap(), "-filter_complex").to_owned(),
                made(&spec).size,
            )
        };
        assert_eq!(
            run(VideoCodec::Av1, 540),
            (
                s("[0:0]null,scale_vaapi=w=1280:h=528[v]"),
                Some((1280, 528))
            )
        );
        // 1728×720 and 1920×800 are whole already.
        assert_eq!(
            run(VideoCodec::Av1, 720),
            (s("[0:0]scale_vaapi=w=-2:h=720[v]"), Some((1728, 720)))
        );
        assert_eq!(run(VideoCodec::Av1, 800).0, "[0:0]null[v]");
        // H.264 crops its padding in the stream itself.
        assert_eq!(
            run(VideoCodec::H264, 540).0,
            "[0:0]scale_vaapi=w=-2:h=540[v]"
        );
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
            cuts: None,
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

    /// Every HLS encode has a keyframe where it is forced and nowhere else:
    /// a VA-API encoder's own, every 120 frames, was the first past an
    /// early cut threshold, and a resumed film was cut a second off the
    /// grid, every segment of it.
    #[test]
    fn an_encoded_hls_run_keys_only_where_it_is_forced() {
        for (start, number) in [(None, 1), (Some(Duration::from_secs(122)), 23)] {
            let args = encoded_hls(start, number);
            assert!(
                has(&args, &["-g", "1000", "-sc_threshold", "0"]),
                "{args:?}"
            );
        }
        let (film, enc) = (film(), encoders(true));
        let output = Output::Hls {
            dir: "/scratch/h/0".into(),
            start_number: 23,
            cuts: None,
        };
        let mut spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_DTS, None, output);
        spec.start = Some(Duration::from_secs(122));
        let args = ffmpeg_args(&spec).unwrap();
        assert_eq!(after(&args, "-g"), "1000", "{args:?}");
    }

    /// A resumed encode's copied sound starts where its picture does, not
    /// at the source's keyframe before the seek; a run from 0, a copied
    /// picture and encoded sound are as they were.
    #[test]
    fn a_resumed_encode_s_copied_audio_starts_at_the_seek() {
        let (film, enc) = (film(), encoders(false));
        let run = |video, audio, start: Option<Duration>| {
            let output = Output::Hls {
                dir: "/scratch/h/0".into(),
                start_number: 632,
                cuts: None,
            };
            let mut spec = spec(&film, &enc, video, audio, None, output);
            spec.start = start;
            ffmpeg_args(&spec).unwrap()
        };
        let at = Some(Duration::from_millis(3_776_000));
        let args = run(TONE_MAPPED_H264, COPY_DTS, at);
        assert_eq!(
            after(&args, "-bsf:a"),
            "noise=drop=lt(pts*tb\\,3776.000)",
            "{args:?}"
        );
        let eac3 = AudioAction::Encode {
            codec: AudioCodec::Eac3,
            channels: 6,
            bitrate: 640_000,
        };
        for args in [
            run(TONE_MAPPED_H264, COPY_DTS, None),
            run(TONE_MAPPED_H264, COPY_DTS, Some(Duration::ZERO)),
            // A copied picture starts at a keyframe, as its sound does.
            run(COPY_HEVC, COPY_DTS, at),
            // Encoded sound is cut at the seek as it is decoded.
            run(TONE_MAPPED_H264, eac3, at),
        ] {
            assert!(!args.contains(&s("-bsf:a")), "{args:?}");
        }
    }

    /// A remux ladder's encoded rung (PX-21) cuts where the copy does: a
    /// keyframe forced at each of the copy's cuts from the run's start on,
    /// the encoder's own keyframes (scene cuts, a GOP's end) off, and the
    /// muxer cutting at every keyframe, as for the copy; no short head.
    #[test]
    fn a_remux_ladder_rung_is_keyed_and_cut_where_the_copy_is() {
        let (film, enc) = (film(), encoders(false));
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 720,
            max_bitrate: Some(3_000_000),
            tone_map: false,
        };
        let cuts = vec![0, 1_001, 2_502, 4_004, 9_009];
        let run = |start: Option<Duration>, start_number| {
            let output = Output::Hls {
                dir: "/scratch/h/0".into(),
                start_number,
                cuts: Some(cuts.clone()),
            };
            let mut spec = spec(&film, &enc, video, COPY_DTS, None, output);
            spec.start = start;
            ffmpeg_args(&spec).unwrap()
        };
        let args = run(None, 1);
        assert_eq!(
            after(&args, "-force_key_frames"),
            "0.000,1.001,2.502,4.004,9.009"
        );
        assert!(
            has(&args, &["-g", "1000", "-sc_threshold", "0"]),
            "{args:?}"
        );
        assert_eq!(after(&args, "-hls_time"), "0.1");
        assert!(!args.contains(&s("-hls_init_time")), "{args:?}");
        assert_eq!(after(&args, "-hls_list_size"), "0");
        // A run from the copy's third cut on: the film's own times (the
        // run is `-copyts`), from there.
        let args = run(Some(Duration::from_millis(2_502)), 3);
        assert_eq!(after(&args, "-force_key_frames"), "2.502,4.004,9.009");
        assert!(args.contains(&s("-copyts")));
    }

    /// The list stops where one argument may not grow past, rather than
    /// failing the run.
    #[test]
    fn a_keyframe_list_is_held_to_one_argument() {
        let cuts: Vec<u64> = (0..100_000).map(|n| n * 1_000).collect();
        let list = keyframes_at(&cuts, None);
        assert!(list.len() <= MAX_KEYFRAME_LIST);
        assert!(list.starts_with("0.000,1.000,"));
        assert!(!list.ends_with(','));
        // Where it stops: the last time in it.
        let last = list.rsplit(',').next().unwrap();
        let through = forced_through(&cuts, None).unwrap();
        assert_eq!(format!("{}.{:03}", through / 1000, through % 1000), last);
        // A list with room for every cut stops nowhere.
        assert_eq!(forced_through(&cuts[..100], None), None);
    }

    /// NVIDIA's encoder is told the same in its own words.
    #[test]
    fn nvenc_adds_no_keyframes_of_its_own() {
        let args = only_forced_keyframes("hevc_nvenc");
        assert_eq!(
            args,
            ["-g", "1000", "-no-scenecut", "1", "-forced-idr", "1"]
        );
        assert_eq!(only_forced_keyframes("av1_vaapi"), ["-g", "1000"]);
    }

    /// libx265 takes neither `-sc_threshold` (ffmpeg reports it unused,
    /// and it cut at every scene change) nor a closed GOP from ffmpeg (its
    /// forced keyframes were CRA): both in its own parameters. libsvtav1
    /// likewise; libx264 takes ffmpeg's own.
    #[test]
    fn software_encoders_add_no_keyframes_of_their_own() {
        assert_eq!(
            only_forced_keyframes("libx265"),
            ["-g", "1000", "-x265-params", "scenecut=0:open-gop=0"]
        );
        assert_eq!(
            only_forced_keyframes("libsvtav1"),
            ["-g", "1000", "-svtav1-params", "scd=0"]
        );
        assert_eq!(
            only_forced_keyframes("libx264"),
            ["-g", "1000", "-sc_threshold", "0"]
        );
    }

    /// A copied video is cut at its own keyframes: no head of its own.
    #[test]
    fn a_copied_hls_video_has_no_short_head() {
        let (film, enc) = (film(), encoders(false));
        let output = Output::Hls {
            dir: "/scratch/h/0".into(),
            start_number: 1,
            cuts: None,
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

    // PGPU-02: one GPU per job, the one that does the most of it.

    /// A host with a GTX 1060 and an AMD GPU, as detection finds it for
    /// `transcode.hardware = ["nvenc", "vaapi"]`: NVENC encodes H.264 and
    /// HEVC, VA-API H.264, HEVC and AV1; NVDEC decodes H.264, 8- and
    /// 10-bit HEVC and VP9, no AV1; VA-API decodes H.264, HEVC and AV1.
    /// Each codec's encoders, and each sample's decoders, in the policy's
    /// order, as detection lists them.
    fn two_gpus(order: [Hardware; 2], tonemap: bool) -> Encoders {
        let encoder = |codec: VideoCodec, hardware: Option<Hardware>| {
            let prefix = match codec {
                VideoCodec::Hevc => "hevc",
                VideoCodec::Av1 => "av1",
                _ => "h264",
            };
            let name = match hardware {
                Some(Hardware::Nvenc) => format!("{prefix}_nvenc"),
                Some(Hardware::Vaapi) => format!("{prefix}_vaapi"),
                Some(Hardware::Qsv) => format!("{prefix}_qsv"),
                None => match codec {
                    VideoCodec::Hevc => s("libx265"),
                    VideoCodec::Av1 => s("libsvtav1"),
                    _ => s("libx264"),
                },
            };
            VideoEncoder {
                codec,
                name,
                hardware,
            }
        };
        let encodes = |hardware: Hardware, codec: VideoCodec| {
            hardware == Hardware::Vaapi || codec != VideoCodec::Av1
        };
        let mut video = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            for hardware in order {
                if encodes(hardware, codec) {
                    video.push(encoder(codec, Some(hardware)));
                }
            }
            video.push(encoder(codec, None));
        }
        let decodes = |hardware: Hardware, codec: VideoCodec, bit_depth: u32| match hardware {
            Hardware::Nvenc => codec != VideoCodec::Av1,
            // VA-API is tried on the 8-bit samples only.
            _ => bit_depth == 8 && codec != VideoCodec::Vp9,
        };
        let mut hardware_decoders = Vec::new();
        for (codec, bit_depth) in [
            (VideoCodec::H264, 8),
            (VideoCodec::Hevc, 8),
            (VideoCodec::Hevc, 10),
            (VideoCodec::Av1, 8),
            (VideoCodec::Vp9, 8),
        ] {
            for hardware in order {
                if decodes(hardware, codec, bit_depth) {
                    hardware_decoders.push(HardwareDecoder {
                        codec,
                        hardware,
                        bit_depth,
                    });
                }
            }
        }
        Encoders {
            video,
            hardware_decoders,
            gpu_tonemap: tonemap,
            gpu_overlay: false,
            // fulcrum's ffmpeg has both (PGPU-03).
            cuda_scale: true,
            cuda_overlay: true,
            ..encoders(false)
        }
    }

    const NVIDIA_FIRST: [Hardware; 2] = [Hardware::Nvenc, Hardware::Vaapi];

    /// `film()`'s video as `codec`, 8-bit SDR at 1080p.
    fn source(codec: &str) -> MediaDescriptor {
        let mut film = sdr_1080p();
        film.video.as_mut().unwrap().codec = codec.into();
        film
    }

    /// An encode to `codec` at 1080p, nothing else asked of it.
    fn to(codec: VideoCodec) -> ConvertedVideo {
        ConvertedVideo::Encode {
            codec,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        }
    }

    /// The encoder and the decoder `spec` runs, as ffmpeg's arguments
    /// and PX-24 have them.
    fn picked(spec: &EncodeSpec<'_>) -> (String, Option<Hardware>) {
        let place = placement(spec).expect("a video encode");
        let args = ffmpeg_args(spec).unwrap();
        assert!(has(&args, &["-c:v", &place.encoder.name]), "{args:?}");
        let made = made(spec);
        assert_eq!(made.encoder.as_ref(), Some(place.encoder));
        assert_eq!(
            made.decoder,
            place.decoder.map_or(Decoder::Software, Decoder::Hardware)
        );
        (place.encoder.name.clone(), place.decoder)
    }

    /// The user's case: NVDEC cannot decode AV1 on a Pascal card, VA-API
    /// can, so the AMD GPU encodes it too rather than NVENC encoding
    /// what the CPU decoded.
    #[test]
    fn an_av1_source_goes_to_the_gpu_that_decodes_it() {
        let (film, enc) = (source("av1"), two_gpus(NVIDIA_FIRST, false));
        for codec in [VideoCodec::H264, VideoCodec::Hevc] {
            let (encoder, decoder) = picked(&spec_with(&film, &enc, to(codec), None));
            assert!(encoder.ends_with("_vaapi"), "{encoder}");
            assert_eq!(decoder, Some(Hardware::Vaapi));
        }
    }

    /// An SDR HEVC film to H.264, nothing to filter: both GPUs do all of
    /// it, so the order decides, and NVIDIA is first.
    #[test]
    fn a_job_both_gpus_do_whole_goes_to_the_first_in_order() {
        let film = source("hevc");
        let enc = two_gpus(NVIDIA_FIRST, false);
        let spec = spec_with(&film, &enc, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_nvenc"), Some(Hardware::Nvenc)));
        let enc = two_gpus([Hardware::Vaapi, Hardware::Nvenc], false);
        let spec = spec_with(&film, &enc, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_vaapi"), Some(Hardware::Vaapi)));
    }

    /// 4K HDR10 HEVC to H.264 at its own height, tone-mapped: with GPU
    /// filters on and `tonemap_vaapi`, VA-API does all of it and NVIDIA
    /// would tone map on the CPU, so VA-API. Without either, both tone
    /// map on the CPU, and the order decides.
    #[test]
    fn a_tone_map_goes_to_the_gpu_that_runs_it() {
        let film = film();
        let video = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 2160,
            max_bitrate: None,
            tone_map: true,
        };
        let tone_maps = two_gpus(NVIDIA_FIRST, true);
        let mut spec = spec_with(&film, &tone_maps, video, None);
        spec.gpu_filters = true;
        assert_eq!(picked(&spec), (s("h264_vaapi"), Some(Hardware::Vaapi)));
        assert_eq!(decode(&spec), Decode::Gpu);
        assert_eq!(made(&spec).tone_map, FilterOn::Gpu);
        // GPU filters off: on the CPU either way.
        spec.gpu_filters = false;
        assert_eq!(picked(&spec), (s("h264_nvenc"), Some(Hardware::Nvenc)));
        // A driver with no tone map.
        let cannot = two_gpus(NVIDIA_FIRST, false);
        let mut spec = spec_with(&film, &cannot, video, None);
        spec.gpu_filters = true;
        assert_eq!(picked(&spec), (s("h264_nvenc"), Some(Hardware::Nvenc)));
        assert_eq!(made(&spec).tone_map, FilterOn::Cpu);
    }

    /// Scaling is a filter too: VA-API scales on the GPU always; NVIDIA
    /// with GPU filters on and `scale_cuda` (PGPU-03), else NVDEC's frames
    /// come back to memory and are scaled on the CPU. So a 4K film made
    /// 1080p goes to VA-API where both decode it and GPU filters are off,
    /// and with them on both do all of it and the order decides.
    #[test]
    fn a_scaled_job_goes_to_the_gpu_that_scales() {
        let mut film = source("hevc");
        let video = film.video.as_mut().unwrap();
        (video.width, video.height) = (3840, 2160);
        let enc = two_gpus(NVIDIA_FIRST, false);
        let mut spec = spec_with(&film, &enc, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_vaapi"), Some(Hardware::Vaapi)));
        assert_eq!(decode(&spec), Decode::Gpu);
        // PGPU-03: GPU filters on, NVIDIA first and scaling on CUDA.
        spec.gpu_filters = true;
        assert_eq!(picked(&spec), (s("h264_nvenc"), Some(Hardware::Nvenc)));
        assert_eq!(decode(&spec), Decode::Gpu);
        assert_eq!(placement(&spec).unwrap().gpu(), GpuUse::Nvidia);
        let vaapi_first = two_gpus([Hardware::Vaapi, Hardware::Nvenc], false);
        let spec = filtered(&film, &vaapi_first, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_vaapi"), Some(Hardware::Vaapi)));
        // Without `scale_cuda`, VA-API's again.
        let no_cuda = Encoders {
            cuda_scale: false,
            ..enc.clone()
        };
        let spec = filtered(&film, &no_cuda, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_vaapi"), Some(Hardware::Vaapi)));
    }

    /// Pictures burned in go to the GPU that overlays them: NVIDIA with
    /// `overlay_cuda`, the AMD GPU (Mesa) having no `overlay_vaapi`,
    /// whatever the order.
    #[test]
    fn a_picture_burn_in_goes_to_the_gpu_that_overlays() {
        let mut film = source("hevc");
        let video = film.video.as_mut().unwrap();
        (video.width, video.height) = (3840, 2160);
        let enc = two_gpus([Hardware::Vaapi, Hardware::Nvenc], false);
        let spec = filtered(
            &film,
            &enc,
            to(VideoCodec::H264),
            burn_in(0, SubtitleFormat::Pgs),
        );
        assert_eq!(picked(&spec), (s("h264_nvenc"), Some(Hardware::Nvenc)));
        assert_eq!(decode(&spec), Decode::Gpu);
        assert_eq!(made(&spec).burn_in, FilterOn::Gpu);
    }

    /// Text burned in runs on the CPU whatever decodes: no GPU does it
    /// all, both decode, and the order decides.
    #[test]
    fn what_no_gpu_runs_leaves_it_to_the_order() {
        let film = source("hevc");
        let enc = two_gpus(NVIDIA_FIRST, true);
        let mut spec = spec_with(
            &film,
            &enc,
            to(VideoCodec::H264),
            burn_in(1, SubtitleFormat::Srt),
        );
        spec.gpu_filters = true;
        assert_eq!(picked(&spec), (s("h264_nvenc"), Some(Hardware::Nvenc)));
    }

    /// A family that encodes but cannot decode loses to one that does
    /// both, whatever the order: here NVDEC was found to decode nothing.
    #[test]
    fn encoding_and_decoding_beats_encoding_alone() {
        let film = source("h264");
        let enc = Encoders {
            hardware_decoders: two_gpus(NVIDIA_FIRST, false)
                .hardware_decoders
                .into_iter()
                .filter(|d| d.hardware == Hardware::Vaapi)
                .collect(),
            ..two_gpus(NVIDIA_FIRST, false)
        };
        let spec = spec_with(&film, &enc, to(VideoCodec::Hevc), None);
        assert_eq!(picked(&spec), (s("hevc_vaapi"), Some(Hardware::Vaapi)));
    }

    /// A 10-bit source goes to the family that decodes 10-bit: NVDEC's
    /// tests here decoded 8-bit HEVC only.
    #[test]
    fn a_10_bit_source_goes_to_the_family_that_decodes_10_bit() {
        let mut film = source("hevc");
        let video = film.video.as_mut().unwrap();
        video.bit_depth = 10;
        video.pixel_format = Some("yuv420p10le".into());
        let enc = two_gpus(NVIDIA_FIRST, false);
        let spec = spec_with(&film, &enc, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_nvenc"), Some(Hardware::Nvenc)));
        let eight_bit = Encoders {
            hardware_decoders: enc
                .hardware_decoders
                .iter()
                .filter(|d| d.bit_depth == 8)
                .cloned()
                .collect(),
            ..enc.clone()
        };
        let spec = spec_with(&film, &eight_bit, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_vaapi"), Some(Hardware::Vaapi)));
    }

    /// No GPU decodes the source (VP9 on VA-API, here alone): each only
    /// encodes, and the order decides.
    #[test]
    fn where_no_gpu_decodes_the_order_decides_the_encoder() {
        let film = source("vp9");
        let enc = Encoders {
            hardware_decoders: Vec::new(),
            ..two_gpus(NVIDIA_FIRST, false)
        };
        let spec = spec_with(&film, &enc, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("h264_nvenc"), None));
    }

    /// No GPU encodes the codec: software, decoding on the first family
    /// in order that decodes the source, NVDEC as VA-API.
    #[test]
    fn where_no_gpu_encodes_software_does_decoding_on_the_first_that_can() {
        let film = source("hevc");
        let enc = Encoders {
            video: two_gpus(NVIDIA_FIRST, false)
                .video
                .into_iter()
                .filter(|e| e.codec != VideoCodec::Av1 || e.hardware.is_none())
                .collect(),
            ..two_gpus(NVIDIA_FIRST, false)
        };
        let spec = spec_with(&film, &enc, to(VideoCodec::Av1), None);
        assert_eq!(picked(&spec), (s("libsvtav1"), Some(Hardware::Nvenc)));
        // AV1, which NVDEC lacks, on VA-API.
        let av1 = source("av1");
        let spec = spec_with(&av1, &enc, to(VideoCodec::Av1), None);
        assert_eq!(picked(&spec), (s("libsvtav1"), Some(Hardware::Vaapi)));
    }

    /// `transcode.hardware = "none"`: detection finds software encoders
    /// and no GPU decoders, so all of it is software.
    #[test]
    fn hardware_none_is_software_throughout() {
        let film = source("hevc");
        let enc = encoders(false);
        let spec = spec_with(&film, &enc, to(VideoCodec::H264), None);
        assert_eq!(picked(&spec), (s("libx264"), None));
        assert_eq!(placement(&spec).unwrap().gpu(), GpuUse::None);
    }

    /// A film whose GPU decode failed before runs on the encoder the
    /// whole-job pick chose, decoding in software: the rerun changes the
    /// decode, not the GPU.
    #[test]
    fn a_failed_gpu_decode_keeps_the_picked_encoder() {
        let (film, enc) = (source("av1"), two_gpus(NVIDIA_FIRST, false));
        let mut spec = spec_with(&film, &enc, to(VideoCodec::H264), None);
        spec.hardware_decode = false;
        assert_eq!(picked(&spec), (s("h264_vaapi"), None));
        assert_eq!(decode(&spec), Decode::Software);
        assert_eq!(placement(&spec).unwrap().gpu(), GpuUse::Vaapi);
    }

    /// The run is confined to the picked family's GPU and no other: the
    /// encoder's, or for a software encode the decoder's.
    #[test]
    fn the_run_uses_the_picked_family_s_gpu() {
        let enc = two_gpus(NVIDIA_FIRST, false);
        let av1 = source("av1");
        let hevc = source("hevc");
        let gpu = |film: &MediaDescriptor, enc: &Encoders, video| {
            placement(&spec_with(film, enc, video, None)).unwrap().gpu()
        };
        assert_eq!(gpu(&av1, &enc, to(VideoCodec::H264)), GpuUse::Vaapi);
        assert_eq!(gpu(&hevc, &enc, to(VideoCodec::H264)), GpuUse::Nvidia);
        let software_av1 = Encoders {
            video: enc
                .video
                .iter()
                .filter(|e| e.codec != VideoCodec::Av1 || e.hardware.is_none())
                .cloned()
                .collect(),
            ..enc.clone()
        };
        assert_eq!(
            gpu(&hevc, &software_av1, to(VideoCodec::Av1)),
            GpuUse::Nvidia
        );
        assert_eq!(gpu(&av1, &software_av1, to(VideoCodec::Av1)), GpuUse::Vaapi);
    }

    // PGPU-04: the tone map on the GPU with libplacebo.

    use spoolframe_media::encoders::Libplacebo;

    /// libplacebo's tone map to 1080p, as the jobs write it.
    const PLACEBO_1080: &str = "libplacebo=w=-2:h=1080:tonemapping=bt.2390:colorspace=bt709:\
                                color_primaries=bt709:color_trc=bt709:range=tv:format=nv12";

    /// `encoders`, libplacebo tone mapping on `family` with `device`.
    fn placebo(encoders: Encoders, family: Hardware, device: VulkanDevice) -> Encoders {
        Encoders {
            libplacebo: vec![Libplacebo {
                hardware: family,
                device,
            }],
            ..encoders
        }
    }

    /// A GTX 1060 whose libplacebo shares ffmpeg's Vulkan device.
    fn nvidia_placebo() -> Encoders {
        placebo(nvidia(), Hardware::Nvenc, VulkanDevice::Shared)
    }

    /// An AMD GPU (VA-API, Mesa: no tonemap_vaapi) whose libplacebo makes
    /// its own device.
    fn amd_placebo() -> Encoders {
        placebo(with_decoders(true), Hardware::Vaapi, VulkanDevice::Own)
    }

    /// PGPU-06: Dolby Vision profile 5 not tone mapped is made HDR10 by
    /// libplacebo (its RPU applied), in 10 bits, encoded Main 10 with the
    /// HDR colours said; tone mapped, it goes through libplacebo whatever
    /// `transcode.tone_map` says, the only tone map that applies the RPU.
    #[test]
    fn dolby_vision_5_goes_through_libplacebo() {
        let film = dv_film(5);
        let mut enc = amd_placebo();
        enc.ten_bit = enc
            .video
            .iter()
            .filter(|e| e.codec == VideoCodec::Hevc)
            .cloned()
            .collect();
        let kept = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        };
        let spec = spec_with(&film, &enc, kept, None);
        assert_eq!(
            libplacebo(&spec),
            Some((Hardware::Vaapi, VulkanDevice::Own))
        );
        let args = ffmpeg_args(&spec).unwrap();
        let graph = after(&args, "-filter_complex");
        assert!(graph.contains("apply_dolbyvision=1"), "{graph}");
        assert!(graph.contains("color_trc=smpte2084"), "{graph}");
        assert!(graph.ends_with("format=p010,hwupload[v]"), "{graph}");
        assert!(has(&args, &["-profile:v", "main10"]));
        assert!(has(&args, &["-color_trc", "smpte2084"]));
        // Tone mapped with `tone_map = "cpu"`: libplacebo still.
        let mut mapped = spec_with(&film, &enc, TONE_MAPPED_H264, None);
        mapped.tone_map = ToneMapSetting::Cpu;
        assert_eq!(
            libplacebo(&mapped),
            Some((Hardware::Vaapi, VulkanDevice::Own))
        );
        let graph = s(after(&ffmpeg_args(&mapped).unwrap(), "-filter_complex"));
        assert!(graph.contains("tonemapping=bt.2390"), "{graph}");
    }

    /// NVIDIA, shared: the 4K HDR10 HEVC film decoded on ffmpeg's NVIDIA
    /// Vulkan device (`-hwaccel vulkan`, frames kept), tone mapped and
    /// scaled by libplacebo there, downloaded in 8 bits to NVENC: the
    /// measured path. All on the GPU; its jail uses Vulkan, on whichever
    /// GPU the NVIDIA jail opens.
    #[test]
    fn nvidia_tone_maps_with_libplacebo_on_ffmpeg_s_vulkan_device() {
        let (film, enc) = (film(), nvidia_placebo());
        let mut spec = spec_with(&film, &enc, TONE_MAPPED_H264, None);
        spec.gpu_filters = true;
        spec.start = Some(Duration::from_secs(600));
        assert_eq!(
            libplacebo(&spec),
            Some((Hardware::Nvenc, VulkanDevice::Shared))
        );
        assert_eq!(decode(&spec), Decode::Gpu);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(
            has(
                &args,
                &[
                    "-init_hw_device",
                    "vulkan=vk:NVIDIA",
                    "-filter_hw_device",
                    "vk",
                    "-hwaccel",
                    "vulkan",
                    "-hwaccel_device",
                    "vk",
                    "-hwaccel_output_format",
                    "vulkan",
                    "-ss",
                ]
            ),
            "{args:?}"
        );
        assert!(!has(&args, &["-hwaccel", "cuda"]), "{args:?}");
        assert_eq!(
            after(&args, "-filter_complex"),
            format!("[0:0]{PLACEBO_1080},hwdownload,format=nv12[v]")
        );
        assert!(has(&args, &["-c:v", "h264_nvenc"]));
        assert_eq!(
            tone_mapper(&spec),
            Some(ToneMapper::Libplacebo(VulkanDevice::Shared))
        );
        let made = made(&spec);
        assert_eq!(made.decoder, Decoder::Vulkan);
        assert_eq!(made.tone_map, FilterOn::Gpu);
        assert_eq!(
            made.tone_mapper,
            Some(ToneMapper::Libplacebo(VulkanDevice::Shared))
        );
        assert_eq!(made.size, Some((1920, 1080)));
        assert!(all_on_gpu(&spec));
        assert_eq!(vulkan(&spec), Some(Vulkan::default()));
        assert!(cpu_instead_of_gpu(&spec).is_empty());
    }

    /// AMD, own device: decoded on VA-API into memory, tone mapped and
    /// scaled by libplacebo's own device (the filters' device being
    /// VA-API's, it sees no Vulkan one), uploaded to `h264_vaapi`. Its
    /// jail's Vulkan names the render node's GPU.
    #[test]
    fn amd_tone_maps_with_libplacebo_s_own_device_through_memory() {
        let (film, enc) = (film(), amd_placebo());
        let mut spec = spec_with(&film, &enc, TONE_MAPPED_H264, None);
        spec.gpu_filters = true;
        assert_eq!(decode(&spec), Decode::Hardware);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(
            has(
                &args,
                &[
                    "-vaapi_device",
                    "/dev/dri/renderD128",
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_device",
                    "/dev/dri/renderD128",
                    "-i",
                ]
            ),
            "{args:?}"
        );
        assert!(!args.iter().any(|a| a.starts_with("vulkan")), "{args:?}");
        assert_eq!(
            after(&args, "-filter_complex"),
            format!("[0:0]{PLACEBO_1080},format=nv12,hwupload[v]")
        );
        assert!(has(&args, &["-c:v", "h264_vaapi"]));
        let made = made(&spec);
        assert_eq!(made.decoder, Decoder::Hardware(Hardware::Vaapi));
        assert_eq!(made.tone_map, FilterOn::Gpu);
        assert_eq!(
            made.tone_mapper,
            Some(ToneMapper::Libplacebo(VulkanDevice::Own))
        );
        assert!(all_on_gpu(&spec));
        assert_eq!(
            vulkan(&spec),
            Some(Vulkan::on(Path::new("/dev/dri/renderD128")))
        );
    }

    /// AMD with `experimental.libplacebo_shared_on_amd` where it works:
    /// VA-API's frames kept, mapped to a Vulkan device derived from the
    /// VA-API one, tone mapped, downloaded in 8 bits and uploaded to the
    /// VA-API device found again from the Vulkan one.
    #[test]
    fn amd_shared_maps_vaapi_frames_to_a_derived_vulkan_device() {
        let film = film();
        let enc = placebo(with_decoders(true), Hardware::Vaapi, VulkanDevice::Shared);
        let mut spec = spec_with(&film, &enc, TONE_MAPPED_H264, None);
        spec.gpu_filters = true;
        assert_eq!(decode(&spec), Decode::Gpu);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(
            has(
                &args,
                &[
                    "-init_hw_device",
                    "vaapi=va:/dev/dri/renderD128",
                    "-init_hw_device",
                    "vulkan=vk@va",
                    "-filter_hw_device",
                    "vk",
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_device",
                    "va",
                    "-hwaccel_output_format",
                    "vaapi",
                    "-i",
                ]
            ),
            "{args:?}"
        );
        assert!(!args.contains(&s("-vaapi_device")), "{args:?}");
        assert_eq!(
            after(&args, "-filter_complex"),
            format!(
                "[0:0]hwmap=derive_device=vulkan,{PLACEBO_1080},hwdownload,format=nv12,\
                 hwupload=derive_device=vaapi[v]"
            )
        );
        assert_eq!(
            made(&spec).tone_mapper,
            Some(ToneMapper::Libplacebo(VulkanDevice::Shared))
        );
    }

    /// `"auto"` tone maps with libplacebo whatever GPU filters say; a
    /// forced method is used whatever they say, and `"cpu"` keeps the
    /// CPU's.
    #[test]
    fn auto_takes_libplacebo_whatever_gpu_filters_say() {
        let (film, enc) = (film(), nvidia_placebo());
        let graph = |tone_map: ToneMapSetting, gpu_filters: bool| {
            let mut spec = spec_with(&film, &enc, TONE_MAPPED_H264, None);
            spec.tone_map = tone_map;
            spec.gpu_filters = gpu_filters;
            s(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"))
        };
        assert!(graph(ToneMapSetting::Auto, false).contains("libplacebo="));
        assert!(graph(ToneMapSetting::Auto, true).contains("libplacebo="));
        assert!(graph(ToneMapSetting::Libplacebo, false).contains("libplacebo="));
        assert!(graph(ToneMapSetting::Cpu, true).contains("tonemap=tonemap=hable"));
        // "gpu" is the driver's own, which NVIDIA has not.
        assert!(graph(ToneMapSetting::Gpu, true).contains("tonemap=tonemap=hable"));
    }

    /// On a GPU with both, auto takes the driver's own tone map first
    /// (Intel's `tonemap_vaapi`, as before); `"libplacebo"` takes
    /// libplacebo; a forced method the GPU lacks is the CPU's, noted once.
    #[test]
    fn the_driver_s_own_tone_map_comes_first_under_auto() {
        let film = film();
        let both = placebo(
            with_filters(true, false),
            Hardware::Vaapi,
            VulkanDevice::Own,
        );
        let mut spec = spec_with(&film, &both, TONE_MAPPED_H264, None);
        // The driver's own is a GPU filter: off, auto takes libplacebo.
        spec.gpu_filters = false;
        assert_eq!(
            tone_mapper(&spec),
            Some(ToneMapper::Libplacebo(VulkanDevice::Own))
        );
        spec.gpu_filters = true;
        assert_eq!(tone_mapper(&spec), Some(ToneMapper::Vaapi));
        assert!(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex").contains("tonemap_vaapi"));
        assert_eq!(tone_map_note(&spec), None);
        spec.tone_map = ToneMapSetting::Libplacebo;
        assert_eq!(
            tone_mapper(&spec),
            Some(ToneMapper::Libplacebo(VulkanDevice::Own))
        );
        let amd = amd_placebo();
        let mut forced = spec_with(&film, &amd, TONE_MAPPED_H264, None);
        forced.tone_map = ToneMapSetting::Gpu;
        assert_eq!(tone_mapper(&forced), Some(ToneMapper::Cpu));
        assert!(
            tone_map_note(&forced).is_some_and(|note| note.contains("tonemap_vaapi")),
            "{:?}",
            tone_map_note(&forced)
        );
        let no_placebo = with_decoders(true);
        let mut forced = spec_with(&film, &no_placebo, TONE_MAPPED_H264, None);
        forced.tone_map = ToneMapSetting::Libplacebo;
        assert_eq!(tone_mapper(&forced), Some(ToneMapper::Cpu));
        assert!(
            tone_map_note(&forced).is_some_and(|note| note.contains("\"libplacebo\"")),
            "{:?}",
            tone_map_note(&forced)
        );
    }

    /// What is burned in goes over libplacebo's output on the CPU, at the
    /// size written: pictures scaled to it and laid over before NVENC;
    /// text drawn by libass before the upload to VA-API. The tone map
    /// stays on the GPU, so the job is not all there.
    #[test]
    fn burn_in_with_libplacebo_is_laid_over_its_output_on_the_cpu() {
        let film = film();
        let enc = nvidia_placebo();
        let mut pictures = spec_with(
            &film,
            &enc,
            TONE_MAPPED_H264,
            burn_in(0, SubtitleFormat::Pgs),
        );
        pictures.gpu_filters = true;
        assert_eq!(
            after(&ffmpeg_args(&pictures).unwrap(), "-filter_complex"),
            format!(
                "[0:2]scale=1920:1080[subs];[0:0]{PLACEBO_1080},hwdownload,format=nv12[main];\
                 [main][subs]overlay[v]"
            )
        );
        let made = made(&pictures);
        assert_eq!(made.tone_map, FilterOn::Gpu);
        assert_eq!(made.burn_in, FilterOn::Cpu);
        assert!(!all_on_gpu(&pictures));
        assert_eq!(
            cpu_instead_of_gpu(&pictures),
            ["pictures are burned in on the CPU after libplacebo's tone map"]
        );

        let enc = amd_placebo();
        let mut text = spec_with(
            &film,
            &enc,
            TONE_MAPPED_H264,
            burn_in(1, SubtitleFormat::Srt),
        );
        text.gpu_filters = true;
        assert_eq!(
            after(&ffmpeg_args(&text).unwrap(), "-filter_complex"),
            format!(
                "[0:0]{PLACEBO_1080},subtitles=filename='/media/Film.mkv':si=1,\
                 format=nv12,hwupload[v]"
            )
        );
        assert!(!all_on_gpu(&text));
    }

    /// NVIDIA shared, a source Vulkan Video is not trusted with: decoded
    /// on NVDEC into memory (VP9) or in software (AV1 on Pascal), and
    /// uploaded to the Vulkan device for libplacebo.
    #[test]
    fn a_source_not_decoded_on_vulkan_is_uploaded_to_it() {
        let enc = nvidia_placebo();
        for (codec, decoded) in [("vp9", Decode::Hardware), ("av1", Decode::Software)] {
            let mut film = film();
            let video = film.video.as_mut().unwrap();
            video.codec = codec.into();
            video.bit_depth = 8;
            video.pixel_format = Some("yuv420p".into());
            let mut spec = spec_with(&film, &enc, TONE_MAPPED_H264, None);
            spec.gpu_filters = true;
            assert_eq!(decode(&spec), decoded, "{codec}");
            let args = ffmpeg_args(&spec).unwrap();
            assert!(!args.contains(&s("vulkan")), "{args:?}");
            assert_eq!(
                args.windows(2).any(|w| w == ["-hwaccel", "cuda"]),
                decoded == Decode::Hardware,
                "{args:?}"
            );
            assert!(!args.contains(&s("-hwaccel_output_format")), "{args:?}");
            assert_eq!(
                after(&args, "-filter_complex"),
                format!("[0:0]hwupload,{PLACEBO_1080},hwdownload,format=nv12[v]")
            );
            assert!(!all_on_gpu(&spec) || decoded == Decode::Hardware);
        }
    }

    /// A run after the GPU failed for the file (`hardware_decode` off)
    /// tone maps on the CPU, with no Vulkan: what a libplacebo failure
    /// runs again as.
    #[test]
    fn a_run_after_a_gpu_failure_tone_maps_on_the_cpu() {
        let (film, enc) = (film(), nvidia_placebo());
        let mut spec = spec_with(&film, &enc, TONE_MAPPED_H264, None);
        spec.gpu_filters = true;
        spec.hardware_decode = false;
        assert_eq!(libplacebo(&spec), None);
        assert_eq!(vulkan(&spec), None);
        assert_eq!(tone_mapper(&spec), Some(ToneMapper::Cpu));
        let graph = s(after(&ffmpeg_args(&spec).unwrap(), "-filter_complex"));
        assert!(graph.contains("tonemap=tonemap=hable"), "{graph}");
        assert_eq!(tone_map_note(&spec), None);
    }

    /// HDR kept (HEVC out, no tone map) runs no libplacebo, as before.
    #[test]
    fn hdr_kept_runs_no_libplacebo() {
        let (film, enc) = (film(), nvidia_placebo());
        let hevc = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        };
        let mut spec = spec_with(&film, &enc, hevc, None);
        spec.gpu_filters = true;
        assert_eq!(libplacebo(&spec), None);
        assert_eq!(tone_mapper(&spec), None);
        let args = ffmpeg_args(&spec).unwrap();
        assert!(args.iter().all(|a| !a.contains("libplacebo")), "{args:?}");
        assert_eq!(made(&spec).tone_mapper, None);
        // Its jail has no Vulkan: only a libplacebo run's has.
        assert_eq!(vulkan(&spec), None);
    }

    /// PGPU-02's ranking counts a GPU tone map as on the GPU: with the
    /// AMD GPU first and neither tonemap_vaapi nor libplacebo there, a
    /// tone-mapped film goes to NVIDIA, which runs libplacebo, as the
    /// one that does all of it; once AMD runs libplacebo too, the order
    /// decides again.
    #[test]
    fn a_gpu_tone_map_counts_toward_the_whole_job() {
        let film = film();
        let to = |encoders: &Encoders| {
            let mut spec = spec_with(&film, encoders, TONE_MAPPED_H264, None);
            spec.gpu_filters = true;
            placement(&spec).unwrap().encoder.name.clone()
        };
        let amd_first = [Hardware::Vaapi, Hardware::Nvenc];
        assert_eq!(to(&two_gpus(amd_first, false)), "h264_vaapi");
        let nvidia_only = placebo(
            two_gpus(amd_first, false),
            Hardware::Nvenc,
            VulkanDevice::Shared,
        );
        assert_eq!(to(&nvidia_only), "h264_nvenc");
        let both = Encoders {
            libplacebo: vec![
                Libplacebo {
                    hardware: Hardware::Vaapi,
                    device: VulkanDevice::Own,
                },
                Libplacebo {
                    hardware: Hardware::Nvenc,
                    device: VulkanDevice::Shared,
                },
            ],
            ..two_gpus(amd_first, false)
        };
        assert_eq!(to(&both), "h264_vaapi");
    }
}
