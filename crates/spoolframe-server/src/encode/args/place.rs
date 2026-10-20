//! Where a run's parts go: the decode, the GPU or CPU for each filter,
//! the encoder, and the share of a card the run takes (PGPU-02).

use std::path::{Path, PathBuf};

use spoolframe_media::encoders::{
    Encoders, Hardware, ToneMapSetting, ToneMapper, VideoEncoder, VulkanDevice,
};
use spoolframe_media::sandbox::{GpuUse, Vulkan};
use spoolframe_media::{MediaDescriptor, SubtitleKind, SubtitleStream, VideoStream, codecs};
use spoolframe_protocol::{ConvertedSubtitle, ConvertedVideo, SubtitleAction};
use spoolframe_types::{HdrFormat, VideoCodec};

use super::*;

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
/// detection's family order among equals (detection lists each
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

/// The spec a plan asks [`placement`] (and [`crate::encode::pick::choose`])
/// about: no file and no run behind it. Tone mapped as
/// `transcode.tone_map = "auto"` (PGPU-04).
pub fn planning_spec<'a>(
    encoders: &'a Encoders,
    descriptor: &'a MediaDescriptor,
    video: ConvertedVideo,
    subtitle: Option<ConvertedSubtitle>,
    gpu_filters: bool,
) -> EncodeSpec<'a> {
    EncodeSpec {
        inputs: Inputs::film(Path::new("")),
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
    }
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
    placement(&planning_spec(
        encoders,
        descriptor,
        video,
        subtitle,
        gpu_filters,
    ))
    .map(|place| place.encoder.clone())
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
pub(super) fn kept_transfer(stream: &VideoStream) -> Option<&'static str> {
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
pub(super) fn burned<'a>(spec: &EncodeSpec<'a>) -> Result<Option<&'a SubtitleStream>, String> {
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
pub(super) fn needs_rpu(spec: &EncodeSpec<'_>) -> bool {
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
pub(super) fn scaled(width: u32, height: u32, to: u32) -> (u32, u32) {
    if to >= height || height == 0 {
        return (width, height);
    }
    let half = (u64::from(width) * u64::from(to) + u64::from(height)) / (2 * u64::from(height));
    ((half * 2) as u32, to)
}

/// The most one `scale_vaapi` shrinks a side by. AMD's VPE (Mesa
/// radeonsi 26.1.8 on a Radeon 890M, ffmpeg 9.0.1) crashes in
/// `si_vpe_set_plane_info` on any step past it: 2160p to 528 lines
/// (4.09×) did 20 times in 20; to 540 (4×), and 2160p to 1056 then to
/// 528, never. Every VA-API scale keeps to it: a step more costs little.
const VAAPI_MAX_DOWNSCALE: u32 = 4;

/// The `scale_vaapi` steps a VA-API scale from `from` to `to` takes
/// first, so none shrinks a side past [`VAAPI_MAX_DOWNSCALE`]: the
/// picture halved, to even sizes, until `to` is within it. None for a
/// scale within it.
pub(super) fn vaapi_steps(from: (u32, u32), to: (u32, u32)) -> Vec<String> {
    let (mut w, mut h) = from;
    let mut steps = Vec::new();
    while to.0 > 0 && to.1 > 0 && (w > to.0 * VAAPI_MAX_DOWNSCALE || h > to.1 * VAAPI_MAX_DOWNSCALE)
    {
        w = (w / 2) & !1;
        h = (h / 2) & !1;
        steps.push(format!("scale_vaapi=w={w}:h={h}"));
    }
    steps
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
pub(super) fn whole_av1_size(
    spec: &EncodeSpec<'_>,
    stream: &VideoStream,
    height: u32,
) -> Option<(u32, u32)> {
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
