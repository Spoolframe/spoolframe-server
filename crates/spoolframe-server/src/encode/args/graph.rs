//! ffmpeg's filter graphs: on the CPU, on VA-API, on CUDA, and through
//! libplacebo on Vulkan (PGPU-04).

use std::path::Path;
use std::time::Duration;

use spoolframe_media::encoders::{
    Hardware, NVIDIA_VULKAN, TONEMAP_VAAPI, VulkanDevice, hwaccel_args, libplacebo_filter,
};
use spoolframe_media::{SubtitleKind, SubtitleSource, SubtitleStream, VideoStream};
use spoolframe_protocol::ConvertedVideo;
use spoolframe_types::VideoCodec;

use super::*;

/// HDR to SDR (BT.709) with the Hable curve, in software: it works for any
/// source and any encoder, where `tonemap_vaapi` needs a VA-API decode.
pub(crate) const TONE_MAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
                        tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,\
                        format=yuv420p";

/// `v` as an argument.
pub(crate) fn s(v: impl ToString) -> String {
    v.to_string()
}

/// A path for a filter argument (`subtitles=filename=…`). Filter graphs
/// have two layers of escaping; rather than get them right for every name
/// in a library, the job links its inputs into its own directory under
/// plain names, and a path that would need escaping is refused.
/// The characters [`filter_path`] refuses in a path: ffmpeg's filter
/// syntax would read them as its own.
pub(crate) const FILTER_SPECIAL: [char; 7] = ['\'', ':', '\\', ',', ';', '[', ']'];

/// `path` as a filter option, or why not, naming `what` the path is: the
/// encode's scratch space or a subtitle file (Review 2026-10-11 N5.1).
pub(super) fn filter_path(path: &Path, what: &str) -> Result<String, String> {
    let raw = path.display().to_string();
    if raw.contains(FILTER_SPECIAL) {
        // Not the path itself: the reason reaches the client (PX-09
        // finding 23).
        return Err(format!(
            "{what}'s path is not one ffmpeg's filters can take"
        ));
    }
    Ok(format!("'{raw}'"))
}

pub(super) fn seconds(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// The video's filters on the CPU: burn-in, tone mapping and scale, and
/// the upload to a VA-API encoder.
pub(super) fn cpu_graph(
    spec: &EncodeSpec<'_>,
    stream: &VideoStream,
    codec: VideoCodec,
    height: u32,
    tone_map: bool,
    vaapi: bool,
    ten_bits: bool,
) -> Result<String, String> {
    let mut chain = Vec::new();
    let mut source = format!("[0:{}]", stream.index);
    if let Some(sub) = spec.subtitle
        && let Some(picked) = burned(spec)?
    {
        match (&picked.source, picked.kind) {
            (source_of, SubtitleKind::Bitmap) => {
                let pictures = pictures_input(spec, source_of)?;
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

/// The video's filters on the GPU, for frames decoded there: the tone map
/// to 8-bit SDR ([`TONEMAP_VAAPI`]); burned-in pictures laid over it
/// (`overlay_vaapi`, their canvas scaled to the video and uploaded); and
/// the scale, to an even width as the CPU's `scale=-2` makes it, and to 8
/// bits, as the CPU's chain uploads `nv12` for every VA-API encode.
///
/// The tone map goes before the overlay (Review 2026-10-11 N5.2): the
/// pictures are SDR, and laid over PQ video before the tone map they would
/// be mapped with it, dimmed or shifted; and `overlay_vaapi` is fed the
/// 8-bit frames [`CpuFilter::PicturesOverTenBits`] counts on, not the
/// decode's 10-bit ones. [`decode`] chose this path, so the GPU runs
/// every filter asked for.
pub(super) fn gpu_graph(
    spec: &EncodeSpec<'_>,
    stream: &VideoStream,
    height: u32,
    tone_map: bool,
    ten_bits: bool,
) -> Result<String, String> {
    let mut chain = Vec::new();
    let mut options = Vec::new();
    if height < stream.height {
        let to = scaled(stream.width, stream.height, height);
        chain.extend(vaapi_steps((stream.width, stream.height), to));
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
            let pictures = format!(
                "[{}]scale={}:{},format=rgba,hwupload[subs];",
                pictures_input(spec, &track.source)?,
                stream.width,
                stream.height
            );
            // Tone mapped first, then the pictures laid over the SDR
            // frames it gives.
            let base = if tone_map {
                format!("{video}{TONEMAP_VAAPI}[sdr];[sdr]")
            } else {
                video
            };
            let mut graph = format!("{pictures}{base}[subs]overlay_vaapi");
            for filter in chain {
                graph.push(',');
                graph.push_str(&filter);
            }
            graph + "[v]"
        }
        None => {
            if tone_map {
                chain.insert(0, s(TONEMAP_VAAPI));
            }
            if chain.is_empty() {
                format!("{video}null[v]")
            } else {
                format!("{video}{}[v]", chain.join(","))
            }
        }
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
pub(super) fn cuda_graph(
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
                pictures_input(spec, &track.source)?,
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
            // The job's link to the film, in the scratch space.
            let input = filter_path(
                spec.inputs.film,
                "the scratch space (transcode.scratch_dir)",
            )?;
            format!("subtitles=filename={input}:si={nth}")
        }
        SubtitleSource::External { path } => {
            // The job's link to it in the scratch space, else the file.
            let path = match spec.inputs.sidecar {
                Some(link) => filter_path(link, "the scratch space (transcode.scratch_dir)")?,
                None => filter_path(path, "a subtitle file")?,
            };
            format!("subtitles=filename={path}")
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
pub(super) fn libplacebo_devices(
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
pub(super) fn libplacebo_graph(
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
                pictures_input(spec, &track.source)?,
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

/// Where a subtitle's pictures, or its stream kept, come from: the
/// film's own stream, or the external file's input.
pub(super) fn pictures_input(
    spec: &EncodeSpec<'_>,
    source: &SubtitleSource,
) -> Result<String, String> {
    match source {
        SubtitleSource::Embedded { index } => Ok(format!("0:{index}")),
        SubtitleSource::External { .. } => input_index(spec, Input::Subtitle)
            .map(|n| format!("{n}:0"))
            .ok_or_else(|| s("subtitle: its file is not read")),
    }
}
