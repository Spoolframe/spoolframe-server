//! The arguments' tests (P2-03).

use std::path::Path;
use std::time::Duration;

use spoolframe_media::encoders::{
    AudioEncoder, Encoders, Hardware, HardwareDecoder, ToneMapSetting, ToneMapper, VideoEncoder,
    VulkanDevice,
};
use spoolframe_media::sandbox::{GpuUse, Vulkan};
use spoolframe_media::{
    AudioStream, MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, VideoStream,
};
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, SubtitleAction,
};
use spoolframe_types::{
    AudioCodec, AudioRole, HdrFormat, SubtitleFormat, SubtitleRole, VideoCodec,
};

use super::{argv::*, graph::*, place::*, *};

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
            parameter_sets_in_record: false,
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
        inputs: Inputs::film(Path::new("/media/Film.mkv")),
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
        spec.inputs.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
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

/// AMD's VPE crashes on a `scale_vaapi` that shrinks a side more
/// than 4×: 2160p to 528 lines is halved first, then scaled; to 540
/// or 1080 lines is one step.
#[test]
fn a_vaapi_scale_past_4x_is_halved_first() {
    let enc = with_decoders(true);
    let film = film();
    let graph = |height| {
        let spec = spec(&film, &enc, plain_h264(height), COPY_DTS, None, hls());
        assert_eq!(decode(&spec), Decode::Gpu);
        after(&ffmpeg_args(&spec).unwrap(), "-filter_complex").to_owned()
    };
    assert_eq!(
        graph(528),
        "[0:0]scale_vaapi=w=1920:h=1080,scale_vaapi=w=-2:h=528:format=nv12[v]"
    );
    assert_eq!(graph(540), "[0:0]scale_vaapi=w=-2:h=540:format=nv12[v]");
    assert_eq!(graph(1080), "[0:0]scale_vaapi=w=-2:h=1080:format=nv12[v]");
}

#[test]
fn no_vaapi_step_shrinks_a_side_past_4x() {
    let size = |step: &str| {
        let (w, h) = step
            .strip_prefix("scale_vaapi=w=")
            .and_then(|r| r.split_once(":h="))
            .unwrap();
        (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap())
    };
    for (from, to) in [
        ((3840, 2160), (960, 528)),
        ((3840, 2160), (640, 352)),
        ((7680, 4320), (426, 240)),
        ((3840, 1600), (1280, 528)),
    ] {
        let mut sizes = vec![from];
        sizes.extend(vaapi_steps(from, to).iter().map(|s| size(s)));
        sizes.push(to);
        for pair in sizes.windows(2) {
            let ((w0, h0), (w1, h1)) = (pair[0], pair[1]);
            assert!(
                w0 <= w1 * 4 && h0 <= h1 * 4,
                "{from:?} to {to:?}: {sizes:?}"
            );
            assert!(w1 % 2 == 0 && h1 % 2 == 0, "{sizes:?}");
        }
    }
    assert!(vaapi_steps((3840, 2160), (960, 540)).is_empty());
    assert!(vaapi_steps((1920, 1080), (1280, 720)).is_empty());
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
        spec.inputs.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
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
        after(&ffmpeg_args(&spec_text).unwrap(), "-filter_complex").contains("subtitles=filename=")
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
    // Tone mapped first, then overlaid on the SDR frames: the pictures
    // are not mapped with the video, and overlay_vaapi is fed 8 bits
    // (Review 2026-10-11 N5.2).
    let tone_mapped = graph.find("tonemap_vaapi").expect("a GPU tone map");
    let overlaid = graph.find("overlay_vaapi").expect("a GPU overlay");
    assert!(tone_mapped < overlaid, "{graph}");
    assert!(graph.contains("[sdr];[sdr][subs]overlay_vaapi"), "{graph}");
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
    let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, progressive())).unwrap();
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
        cuts: Cuts::Own,
        pad_audio: false,
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
    // Sound that starts on time is not padded.
    assert!(!args.contains(&s("-af")));
}

/// Audio that starts after the run's first cut is filled with silence
/// from where the run starts: from 0, or from a seek, in samples at
/// the 48 kHz it is resampled to.
#[test]
fn late_audio_is_padded_from_where_the_run_starts() {
    let (film, enc) = (film(), encoders(false));
    let eac3 = AudioAction::Encode {
        codec: AudioCodec::Eac3,
        channels: 6,
        bitrate: 640_000,
    };
    let padded = || Output::Hls {
        dir: "/scratch/h/0".into(),
        start_number: 1,
        cuts: Cuts::Own,
        pad_audio: true,
    };
    let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, eac3, None, padded())).unwrap();
    assert_eq!(after(&args, "-af"), "aresample=48000:async=1:first_pts=0");
    let sought = EncodeSpec {
        start: Some(Duration::from_millis(3_250)),
        ..spec(&film, &enc, COPY_HEVC, eac3, None, padded())
    };
    let args = ffmpeg_args(&sought).unwrap();
    assert_eq!(
        after(&args, "-af"),
        "aresample=48000:async=1:first_pts=156000"
    );
}

fn hls() -> Output {
    Output::Hls {
        dir: "/scratch/h/0".into(),
        start_number: 1,
        cuts: Cuts::Own,
        pad_audio: false,
    }
}

/// A copied video cut by the segment muxer at the conversion's list:
/// 0, 3.5, 6.083 and 9 s, a run from the start beginning at `from_ms`.
fn listed(start_number: u32, from_ms: i64) -> Output {
    Output::Hls {
        dir: "/scratch/h/0".into(),
        start_number,
        cuts: Cuts::Listed {
            starts: std::sync::Arc::from(vec![0, 3500, 6083, 9000]),
            from_ms,
        },
        pad_audio: false,
    }
}

/// Review 2026-10-11 M2: a copied video whose header can be written
/// before its first packet is cut by the segment muxer at the
/// conversion's own cuts, which it measures from the run's first
/// video packet: here the film's first keyframe, at 0. Its header is
/// `init.mp4`, written once; its list says which segments are done.
#[test]
fn a_listed_copy_is_cut_by_the_segment_muxer_at_the_conversion_s_cuts() {
    let (film, enc) = (film(), encoders(false));
    let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, listed(1, 0))).unwrap();
    assert!(has(&args, &["-f", "segment", "-segment_format", "mp4"]));
    assert_eq!(
        after(&args, "-segment_format_options"),
        "movflags=+frag_custom+dash+delay_moov+frag_discont"
    );
    assert_eq!(
        after(&args, "-segment_header_filename"),
        "/scratch/h/0/init.mp4"
    );
    assert!(has(&args, &["-individual_header_trailer", "0"]));
    assert_eq!(after(&args, "-segment_times"), "3.500,6.083,9.000");
    assert_eq!(after(&args, "-segment_time_delta"), "0.001");
    assert_eq!(after(&args, "-segment_start_number"), "1");
    assert_eq!(after(&args, "-segment_list"), "/scratch/h/0/ffmpeg.m3u8");
    assert!(has(&args, &["-segment_list_type", "m3u8"]));
    assert_eq!(args.last().unwrap(), "/scratch/h/0/seg%d.m4s");
    assert!(!args.iter().any(|a| a.starts_with("-hls")), "{args:?}");
    // As the HLS muxer's: `hvc1`, its sets read into the record.
    assert!(has(&args, &["-tag:v", "hvc1"]));
    assert!(has(&args, &["-bsf:v", "hevc_mp4toannexb"]));
    // From the start, nothing comes before.
    assert!(!args.contains(&s("-copypriorss")));
}

/// The cuts are measured from where the run's video starts: a run
/// from the start of an MP4 cut with stream copy starts 40 ms before
/// 0, one with a late picture at 0.5 s.
#[test]
fn a_listed_copy_from_the_start_measures_from_its_first_keyframe() {
    let (film, enc) = (film(), encoders(false));
    let times = |from_ms| {
        let args = ffmpeg_args(&spec(
            &film,
            &enc,
            COPY_HEVC,
            COPY_DTS,
            None,
            listed(1, from_ms),
        ))
        .unwrap();
        after(&args, "-segment_times").to_owned()
    };
    assert_eq!(times(-40), "3.540,6.123,9.040");
    assert_eq!(times(500), "3.000,5.583,8.500");
}

/// A run from segment 3 starts at its cut, 6.083 s: the cuts after it
/// are measured from there, and numbered on from 3.
#[test]
fn a_resumed_listed_copy_measures_from_its_own_cut() {
    let (film, enc) = (film(), encoders(false));
    let mut resumed = spec(&film, &enc, COPY_HEVC, COPY_DTS, None, listed(3, 0));
    resumed.start = Some(Duration::from_millis(6_082));
    let args = ffmpeg_args(&resumed).unwrap();
    assert_eq!(after(&args, "-segment_times"), "2.917");
    assert_eq!(after(&args, "-segment_start_number"), "3");
    // What the seek lands on before the cut is dropped.
    assert!(has(&args, &["-copypriorss", "0"]));
    // The last segment has no cut after it: one past any film.
    resumed.output = listed(4, 0);
    resumed.start = Some(Duration::from_millis(8_999));
    let args = ffmpeg_args(&resumed).unwrap();
    assert_eq!(after(&args, "-segment_times"), "1000000.000");
}

/// The MP4 muxer writes a Dolby Vision record only with `-strict
/// unofficial`, and the segment muxer does not hand its own on.
#[test]
fn a_listed_dolby_vision_copy_passes_strict_to_the_mp4_muxer() {
    let film = dv_film(8);
    let enc = encoders(false);
    let video = ConvertedVideo::Copy {
        codec: VideoCodec::Hevc,
        dolby_vision: true,
    };
    let args = ffmpeg_args(&spec(&film, &enc, video, COPY_DTS, None, listed(1, 0))).unwrap();
    assert_eq!(
        after(&args, "-segment_format_options"),
        "movflags=+frag_custom+dash+delay_moov+frag_discont:strict=unofficial"
    );
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
    let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, progressive())).unwrap();
    assert!(!args.contains(&s("-bsf:v")), "{args:?}");
}

#[test]
fn only_hevc_in_hls_is_tagged() {
    let (film, enc) = (film(), encoders(false));
    // Matroska has no sample entries.
    let args = ffmpeg_args(&spec(&film, &enc, COPY_HEVC, COPY_DTS, None, progressive())).unwrap();
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
    spec.inputs.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
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
    spec.inputs.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
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
            cuts: Cuts::Own,
            pad_audio: false,
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
        cuts: Cuts::Own,
        pad_audio: false,
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
        cuts: Cuts::Own,
        pad_audio: false,
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
            cuts: Cuts::Own,
            pad_audio: false,
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
            cuts: Cuts::Forced(cuts.clone().into()),
            pad_audio: false,
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
        cuts: Cuts::Own,
        pad_audio: false,
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
        filter_path(Path::new("/scratch/h/0/input"), "the scratch space").unwrap(),
        "'/scratch/h/0/input'"
    );
    assert!(filter_path(Path::new("/m/It's: A Film.srt"), "a subtitle file").is_err());
}

/// An embedded text track burned in from a scratch space whose path
/// ffmpeg's filters misread is refused for the scratch space, not
/// blamed on a subtitle file (Review 2026-10-11 N5.1).
#[test]
fn a_refused_burn_in_names_the_path_that_failed() {
    let why = filter_path(
        Path::new("/srv/scratch:1/h/0/input"),
        "the scratch space (transcode.scratch_dir)",
    )
    .unwrap_err();
    assert!(why.contains("scratch space"), "{why}");
    assert!(!why.contains("subtitle"), "{why}");
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
    spec.inputs.sidecar = Some(Path::new("/scratch/h/0/sidecar"));
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
/// NVENC then VA-API (`HardwarePolicy::Ordered`): NVENC encodes H.264 and
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

/// `HardwarePolicy::None`: detection finds software encoders
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

/// [`film`] with its DTS track an optimized copy's E-AC-3, stream 1 of
/// the copy (PX-37).
fn film_with_copy_audio() -> MediaDescriptor {
    let mut film = film();
    film.audio[0].codec = "eac3".into();
    film
}

const COPY_EAC3: AudioAction = AudioAction::Copy {
    codec: AudioCodec::Eac3,
};

#[test]
fn audio_from_a_copy_is_read_as_a_second_input_sought_as_the_film_is() {
    let (film, enc) = (film_with_copy_audio(), encoders(false));
    let mut spec = spec(
        &film,
        &enc,
        TONE_MAPPED_H264,
        COPY_EAC3,
        None,
        progressive(),
    );
    spec.inputs.audio = Some(Path::new("/job/audio"));
    spec.start = Some(Duration::from_secs(600));
    let args = ffmpeg_args(&spec).unwrap();
    assert!(
        has(&args, &["-ss", "600.000", "-i", "/job/audio"]),
        "{args:?}"
    );
}

#[test]
fn audio_from_a_copy_is_mapped_from_it_and_copied() {
    let (film, enc) = (film_with_copy_audio(), encoders(false));
    let mut spec = spec(
        &film,
        &enc,
        TONE_MAPPED_H264,
        COPY_EAC3,
        None,
        progressive(),
    );
    spec.inputs.audio = Some(Path::new("/job/audio"));
    let args = ffmpeg_args(&spec).unwrap();
    assert!(has(&args, &["-map", "1:1", "-c:a", "copy"]), "{args:?}");
}

/// The subtitle file is the second input, the copy the third.
#[test]
fn audio_from_a_copy_comes_after_a_subtitle_file() {
    let (film, enc) = (film_with_copy_audio(), encoders(false));
    let srt = Some(ConvertedSubtitle {
        index: 2,
        action: SubtitleAction::Copy {
            format: SubtitleFormat::Srt,
        },
    });
    let mut spec = spec(&film, &enc, TONE_MAPPED_H264, COPY_EAC3, srt, progressive());
    spec.inputs.audio = Some(Path::new("/job/audio"));
    let args = ffmpeg_args(&spec).unwrap();
    assert!(has(&args, &["-map", "2:1", "-c:a", "copy"]), "{args:?}");
}

/// A copy's stored video and audio (PX-37's track store): the film first,
/// then the video, then the audio, each mapped from its own input.
#[test]
fn a_stored_video_and_audio_are_mapped_from_their_own_inputs() {
    let (mut film, enc) = (film_with_copy_audio(), encoders(false));
    film.video.as_mut().unwrap().index = 0;
    film.audio[0].index = 0;
    let mut spec = spec(&film, &enc, COPY_HEVC, COPY_EAC3, None, progressive());
    spec.inputs.video = Some(Path::new("/job/video"));
    spec.inputs.audio = Some(Path::new("/job/audio"));
    let args = ffmpeg_args(&spec).unwrap();
    assert!(
        has(&args, &["-i", "/job/video", "-i", "/job/audio"])
            && has(&args, &["-map", "1:0", "-c:v", "copy"])
            && has(&args, &["-map", "2:0", "-c:a", "copy"]),
        "{args:?}"
    );
}

/// A stored track keeps the film's times: every input keeps its own.
#[test]
fn a_run_reading_a_stored_track_keeps_every_input_s_times() {
    let (film, enc) = (film_with_copy_audio(), encoders(false));
    let mut spec = spec(
        &film,
        &enc,
        TONE_MAPPED_H264,
        COPY_EAC3,
        None,
        progressive(),
    );
    spec.inputs.audio = Some(Path::new("/job/audio"));
    assert!(ffmpeg_args(&spec).unwrap().iter().any(|a| a == "-copyts"));
}

#[test]
fn a_stored_video_is_never_encoded() {
    let (film, enc) = (film_with_copy_audio(), encoders(false));
    let mut spec = spec(
        &film,
        &enc,
        TONE_MAPPED_H264,
        COPY_EAC3,
        None,
        progressive(),
    );
    spec.inputs.video = Some(Path::new("/job/video"));
    assert!(ffmpeg_args(&spec).is_err());
}
