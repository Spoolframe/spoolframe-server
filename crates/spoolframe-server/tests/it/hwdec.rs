//! Transcodes decoding on the GPU, with real ffmpeg on a real VA-API render
//! node, confined as the server confines it. Skipped, saying so, on a
//! machine without one (CI): only where a render node is there and
//! detection's own test encode and decode work on it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use spoolframe_media::encoders::{self, Encoders, FfmpegCommand, Hardware, TranscodeSettings};
use spoolframe_media::sandbox::{self, GpuUse, Jail, Limits, Mode, Sandbox, Tool};
use spoolframe_media::{MediaDescriptor, Prober};
use spoolframe_protocol::ConvertedVideo;
use spoolframe_server::encode::args::{Decode, EncodeSpec, Output, decode, ffmpeg_args};
use spoolframe_server::encode::job::{Fallback, Jobs, Run, Written};
use spoolframe_server::encode::{JobLimits, Load, Units};
use spoolframe_types::VideoCodec;

use crate::support::fresh_dir;

/// A render node VA-API encodes and decodes H.264 on, the sandbox with
/// it, and what detection found there.
struct Gpu {
    node: PathBuf,
    sandbox: Sandbox,
    encoders: Encoders,
}

/// The first render node that passes detection's tests, or why none.
async fn gpu(scratch: &Path) -> Result<Gpu, String> {
    let nodes: Vec<PathBuf> = (128..136)
        .map(|n| PathBuf::from(format!("/dev/dri/renderD{n}")))
        .filter(|node| node.exists())
        .collect();
    if nodes.is_empty() {
        return Err("no /dev/dri/renderD12x".into());
    }
    for node in nodes {
        let sandbox = Sandbox::new(
            Mode::Required,
            Vec::new(),
            sandbox::gpu_devices(Some(&node), false),
        )
        .expect("this kernel has Landlock");
        let settings = TranscodeSettings {
            vaapi_device: node.clone(),
            ..TranscodeSettings::default()
        };
        let ffmpeg = FfmpegCommand::new(Tool::new("ffmpeg", sandbox.clone()));
        let encoders = encoders::detect(&ffmpeg, &settings, &scratch.join("detect")).await;
        let encodes = encoders
            .video_encoder(VideoCodec::H264)
            .is_some_and(|e| e.hardware == Some(Hardware::Vaapi));
        if encodes && encoders.decodes(VideoCodec::H264, Hardware::Vaapi) {
            return Ok(Gpu {
                node,
                sandbox,
                encoders,
            });
        }
    }
    Err("no render node encodes and decodes H.264 with VA-API".into())
}

/// The GPU, or `None` after saying why the test is skipped.
async fn gpu_or_skip(test: &str, scratch: &Path) -> Option<Gpu> {
    match gpu(scratch).await {
        Ok(gpu) => Some(gpu),
        Err(why) => {
            eprintln!("{test}: skipped: {why}");
            None
        }
    }
}

/// Runs `program` unconfined, as the test's own tool.
fn run(program: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {program}: {e}"))
}

/// A 3-second 1280x720 H.264 clip in `pixel_format`, encoded in
/// software, described as the server describes it.
async fn clip(
    dir: &Path,
    sandbox: &Sandbox,
    pixel_format: &str,
) -> (PathBuf, Arc<MediaDescriptor>) {
    let film = dir.join(format!("clip-{pixel_format}.mkv"));
    let made = run(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=24:duration=3",
            // Tagged, as films are: tone mapping needs to know the
            // transfer it starts from.
            "-vf",
            "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709",
            "-pix_fmt",
            pixel_format,
            "-c:v",
            "libx264",
            film.to_str().unwrap(),
        ],
    );
    assert!(made.status.success(), "{made:?}");
    let prober = Prober::new(
        Tool::new("ffprobe", sandbox.clone()),
        Prober::DEFAULT_TIMEOUT,
    );
    let descriptor = prober.describe(&film).await.unwrap();
    (film, descriptor)
}

/// The output's frames, decoded, and their height, or why it does not
/// decode.
fn decoded(out: &Path) -> Result<(u32, u32), String> {
    let probe = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames,height",
            "-of",
            "csv=p=0",
            out.to_str().unwrap(),
        ],
    );
    if !probe.status.success() {
        return Err(String::from_utf8_lossy(&probe.stderr).into_owned());
    }
    let text = String::from_utf8_lossy(&probe.stdout);
    let mut fields = text.trim().split(',');
    let height = fields.next().and_then(|f| f.parse().ok());
    let frames = fields.next().and_then(|f| f.parse().ok());
    match (frames, height) {
        (Some(frames), Some(height)) => Ok((frames, height)),
        _ => Err(format!("ffprobe says {text:?}")),
    }
}

/// An encode of `film` to 480p H.264 into `out`, tone-mapped or not.
fn spec<'a>(
    film: &'a Path,
    descriptor: &'a MediaDescriptor,
    gpu: &'a Gpu,
    hardware_decode: bool,
    tone_map: bool,
    out: &Path,
) -> EncodeSpec<'a> {
    EncodeSpec {
        input: film,
        sidecar: None,
        descriptor,
        video: Some(ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 480,
            max_bitrate: Some(2_000_000),
            tone_map,
        }),
        audio: None,
        subtitle: None,
        encoders: &gpu.encoders,
        vaapi_device: &gpu.node,
        hardware_decode,
        gpu_filters: false,
        tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
        start: None,
        output: Output::Progressive {
            file: out.to_path_buf(),
        },
    }
}

/// ffmpeg with `args`, confined as an encode job is.
async fn confined(gpu: &Gpu, film: &Path, dir: &Path, args: &[String]) -> std::process::Output {
    let jail = Jail::new(Limits::encode(1 << 30))
        .input(film)
        .writes(dir)
        .gpu(GpuUse::Vaapi);
    let mut command = Tool::new("ffmpeg", gpu.sandbox.clone())
        .command(&jail)
        .unwrap();
    command.args(args).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("ffmpeg took over a minute")
        .unwrap()
}

/// An encode, inside the sandbox's GPU rules, makes the same film
/// whichever way it decodes: in software, on the GPU into memory for a
/// filter on the CPU (tone mapping) or for libx264, or on the GPU
/// throughout.
#[tokio::test]
async fn an_encode_makes_output_that_decodes_whichever_way_it_decodes() {
    let dir = fresh_dir("vaapi-decode").await;
    let Some(gpu) = gpu_or_skip("vaapi-decode", &dir).await else {
        return;
    };
    let (film, descriptor) = clip(&dir, &gpu.sandbox, "yuv420p").await;
    let software = Encoders {
        video: (gpu.encoders.video.iter())
            .filter(|e| e.hardware.is_none())
            .cloned()
            .collect(),
        ..gpu.encoders.clone()
    };
    for (name, encoders, hardware, tone_map, way) in [
        (
            "vaapi-software",
            &gpu.encoders,
            false,
            false,
            Decode::Software,
        ),
        ("vaapi-memory", &gpu.encoders, true, true, Decode::Hardware),
        ("vaapi-gpu", &gpu.encoders, true, false, Decode::Gpu),
        ("x264-memory", &software, true, false, Decode::Hardware),
    ] {
        let out = dir.join(format!("out-{name}.mkv"));
        let mut spec = spec(&film, &descriptor, &gpu, hardware, tone_map, &out);
        spec.encoders = encoders;
        assert_eq!(decode(&spec), way, "{name} decodes elsewhere");
        let args = ffmpeg_args(&spec).unwrap();
        let ran = confined(&gpu, &film, &dir, &args).await;
        assert!(
            ran.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
        let (frames, height) = decoded(&out).unwrap();
        assert_eq!((frames, height), (72, 480), "{name}");
    }
}

/// A stream the driver refuses fails on the GPU path before its first
/// output: here 10-bit H.264, which no VA-API driver decodes, described as
/// 8-bit so the GPU path is tried. The job runs again decoding in
/// software, and finishes.
#[tokio::test]
async fn a_gpu_decode_that_fails_at_once_finishes_in_software() {
    let dir = fresh_dir("vaapi-decode-fallback").await;
    let Some(gpu) = gpu_or_skip("vaapi-decode-fallback", &dir).await else {
        return;
    };
    let (film, described) = clip(&dir, &gpu.sandbox, "yuv420p10le").await;
    let mut descriptor = (*described).clone();
    let video = descriptor.video.as_mut().unwrap();
    assert_eq!(video.bit_depth, 10);
    video.bit_depth = 8;
    video.pixel_format = Some("yuv420p".into());
    let jobs = Jobs::new(
        JobLimits {
            budget: Units::jobs(1),
            max_remux: 1,
            max_gpu_jobs: 1,
            scratch_dir: dir.join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(600),
            stalled_after: Duration::from_secs(60),
            gpu_filters: false,
            tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            video_codecs: spoolframe_server::config::DEFAULT_VIDEO_CODECS.to_vec(),
        },
        Tool::new("ffmpeg", gpu.sandbox.clone()),
    )
    .unwrap();
    let job_dir = jobs.dir().join("h/0");
    let out = job_dir.join("out.mkv");
    let on_gpu = spec(&film, &descriptor, &gpu, true, false, &out);
    assert_eq!(decode(&on_gpu), Decode::Gpu);
    let args = ffmpeg_args(&on_gpu).unwrap();
    let software = ffmpeg_args(&EncodeSpec {
        hardware_decode: false,
        ..on_gpu.clone()
    })
    .unwrap();
    let job = jobs
        .start(
            job_dir,
            Run {
                args,
                inputs: vec![film.clone()],
                reads: Vec::new(),
                gpu: GpuUse::Vaapi,
                vulkan: None,
                fallback: Some(Fallback {
                    args: software,
                    gpu: GpuUse::Vaapi,
                    file: film.clone(),
                    made: None,
                }),
                made: None,
                card: None,
                ffmpeg: None,
                aligned_until: None,
            },
            Written::Progressive { file: out.clone() },
            Load::Gpu,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let mut progress = job.progress();
    let ended = tokio::time::timeout(
        Duration::from_secs(60),
        progress.wait_for(|p| p.done.is_some()),
    )
    .await
    .expect("the job never ended")
    .unwrap()
    .clone();
    assert_eq!(ended.done, Some(Ok(())), "{:?}", job.stderr());
    assert!(jobs.hardware_decode_failed(&film));
    assert!(!job.command().iter().any(|a| a == "-hwaccel"));
    assert_eq!(decoded(&out).unwrap(), (72, 480));
}

/// With GPU filters on, a 10-bit HDR10 HEVC source is tone mapped on the
/// GPU to 8-bit SDR, where the driver can (Intel's iHD). Mesa's VA-API
/// cannot: skipped there, saying so, as the server would tone map on the
/// CPU.
#[tokio::test]
async fn an_hdr_source_is_tone_mapped_on_the_gpu_where_its_driver_can() {
    let test = "vaapi-tonemap";
    let dir = fresh_dir(test).await;
    let Some(gpu) = gpu_or_skip(test, &dir).await else {
        return;
    };
    if !gpu.encoders.gpu_tonemap {
        eprintln!(
            "{test}: skipped: {} does not tone map on the GPU (tonemap_vaapi)",
            gpu.node.display()
        );
        return;
    }
    let film = dir.join("hdr10.mkv");
    let made = run(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=24:duration=3",
            "-vf",
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
            "-pix_fmt",
            "yuv420p10le",
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error:hdr10=1",
            film.to_str().unwrap(),
        ],
    );
    assert!(made.status.success(), "{made:?}");
    let prober = Prober::new(
        Tool::new("ffprobe", gpu.sandbox.clone()),
        Prober::DEFAULT_TIMEOUT,
    );
    let descriptor = prober.describe(&film).await.unwrap();
    let out = dir.join("sdr.mkv");
    let mut spec = spec(&film, &descriptor, &gpu, true, true, &out);
    spec.gpu_filters = true;
    assert_eq!(decode(&spec), Decode::Gpu);
    let args = ffmpeg_args(&spec).unwrap();
    assert!(args.iter().any(|a| a.contains("tonemap_vaapi")), "{args:?}");
    let ran = confined(&gpu, &film, &dir, &args).await;
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(decoded(&out).unwrap(), (72, 480));
    let probe = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=pix_fmt,color_transfer",
            "-of",
            "csv=p=0",
            out.to_str().unwrap(),
        ],
    );
    let text = String::from_utf8_lossy(&probe.stdout);
    let mut fields = text.trim().split(',');
    assert_eq!(fields.next(), Some("yuv420p"), "8-bit: {text}");
    assert_ne!(fields.next(), Some("smpte2084"), "SDR: {text}");
}
