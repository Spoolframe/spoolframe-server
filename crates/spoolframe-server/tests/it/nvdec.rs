//! NVDEC for NVENC encodes (PGPU-01) and CUDA filters (PGPU-03), end to end
//! on an NVIDIA host: real ffmpeg, confined as the server confines it,
//! detection's probes, and a film encoded to 1080p H.264 on NVENC three
//! ways, with the CPU each took: decoded in software and filtered on the
//! CPU; decoded on NVDEC and filtered on the CPU; decoded on NVDEC and
//! scaled on the GPU (`scale_cuda`, `transcode.gpu_filters` on).
//! Ignored: no NVIDIA GPU here. On one (fulcrum's GTX 1060):
//!
//! ```text
//! CARGO_INCREMENTAL=0 devenv shell -- cargo test -p spoolframe-server --test it \
//!     nvdec -- --ignored --nocapture
//! ```
//!
//! `SPOOLFRAME_NVDEC_FILE=/path/to/4k.mkv` encodes the first 60 s of a
//! real film instead of the generated 4K SDR clip; an HDR film is tone
//! mapped, on the CPU on NVIDIA, so its third run filters on the CPU
//! too. `SPOOLFRAME_NVDEC_SANDBOX=off` runs ffmpeg unconfined, to tell a
//! sandbox refusal from a driver one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use spoolframe_media::Prober;
use spoolframe_media::encoders::{
    self, FfmpegCommand, Hardware, HardwarePolicy, TranscodeSettings,
};
use spoolframe_media::sandbox::{self, GpuUse, Jail, Limits, Mode, Sandbox, Tool};
use spoolframe_protocol::ConvertedVideo;
use spoolframe_server::encode::Decoder;
use spoolframe_server::encode::args::{
    Decode, EncodeSpec, Inputs, Output, decode, ffmpeg_args, gpu_decoder, made,
};
use spoolframe_types::VideoCodec;

use crate::support::fresh_dir;

fn sandbox() -> Sandbox {
    let mode = match std::env::var("SPOOLFRAME_NVDEC_SANDBOX").as_deref() {
        Ok("off") => Mode::Off,
        _ => Mode::Required,
    };
    Sandbox::new(mode, Vec::new(), sandbox::gpu_devices(None, true))
        .expect("this kernel has Landlock")
}

/// A 5-second 4K SDR HEVC clip, 8-bit: what PGPU-03 measures, a film
/// NVIDIA can now scale on the GPU.
fn clip(dir: &Path) -> PathBuf {
    let film = dir.join("sdr-4k.mkv");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=3840x2160:rate=24:duration=5",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error",
        ])
        .arg(&film)
        .output()
        .expect("ffmpeg runs");
    assert!(made.status.success(), "{made:?}");
    film
}

/// The CPU time (user + system) this process's waited-for children have
/// used so far.
fn children_cpu() -> Duration {
    // SAFETY: getrusage fills the struct it is given and keeps nothing.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        assert_eq!(libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage), 0);
        usage
    };
    let time = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    time(usage.ru_utime) + time(usage.ru_stime)
}

/// Runs `args` confined as an encode job is; the CPU it took, and how
/// long it ran.
async fn run(sandbox: &Sandbox, film: &Path, dir: &Path, args: &[String]) -> (Duration, Duration) {
    let jail = Jail::new(Limits::encode(4 << 30))
        .input(film)
        .writes(dir)
        .gpu(GpuUse::Nvidia);
    let mut command = Tool::new("ffmpeg", sandbox.clone()).command(&jail).unwrap();
    command.args(args).kill_on_drop(true);
    let before = children_cpu();
    let started = Instant::now();
    let ran = tokio::time::timeout(Duration::from_secs(600), command.output())
        .await
        .expect("ffmpeg took over ten minutes")
        .unwrap();
    assert!(
        ran.status.success(),
        "{}\n{}",
        args.join(" "),
        String::from_utf8_lossy(&ran.stderr)
    );
    (children_cpu() - before, started.elapsed())
}

/// One of the three ways.
struct Way {
    name: &'static str,
    hardware_decode: bool,
    gpu_filters: bool,
}

const WAYS: [Way; 3] = [
    Way {
        name: "software decode + CPU filters",
        hardware_decode: false,
        gpu_filters: false,
    },
    Way {
        name: "NVDEC + CPU filters",
        hardware_decode: true,
        gpu_filters: false,
    },
    Way {
        name: "NVDEC + CUDA filters",
        hardware_decode: true,
        gpu_filters: true,
    },
];

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an NVIDIA GPU: run by hand on one"]
async fn an_nvenc_encode_decodes_on_nvdec_and_scales_on_cuda() {
    let dir = fresh_dir("nvdec").await;
    let sandbox = sandbox();
    let settings = TranscodeSettings {
        hardware: HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
        ..TranscodeSettings::default()
    };
    let ffmpeg = FfmpegCommand::new(Tool::new("ffmpeg", sandbox.clone()));
    let found = encoders::detect(&ffmpeg, &settings, &dir.join("detect")).await;
    eprintln!("detected: {}", found.summary());
    for failed in &found.failed {
        eprintln!(
            "  failed test {} (lacks {}): {}",
            failed.test, failed.lacks, failed.stderr
        );
    }
    assert_eq!(
        found
            .video_encoder(VideoCodec::H264)
            .and_then(|e| e.hardware),
        Some(Hardware::Nvenc),
        "no NVENC: {}",
        found.summary()
    );
    assert!(
        found.cuda_scale,
        "scale_cuda's test failed (see its stderr above): {}",
        found.summary()
    );

    let given = std::env::var_os("SPOOLFRAME_NVDEC_FILE").map(PathBuf::from);
    let film = given.clone().unwrap_or_else(|| clip(&dir));
    let prober = Prober::new(
        Tool::new("ffprobe", sandbox.clone()),
        Prober::DEFAULT_TIMEOUT,
    );
    let descriptor = prober.describe(&film).await.unwrap();
    let video = descriptor.video.clone().expect("a video stream");
    let hdr = video.hdr.is_some();
    assert!(
        found.decodes_depth(VideoCodec::Hevc, video.bit_depth, Hardware::Nvenc)
            || video.codec != "hevc",
        "NVDEC did not decode {}-bit HEVC: {}",
        video.bit_depth,
        found.summary()
    );
    let mut cpu = Vec::new();
    let mut lines = Vec::new();
    for (i, way) in WAYS.iter().enumerate() {
        let out = dir.join(format!("out-{i}.mkv"));
        let spec = EncodeSpec {
            inputs: Inputs::film(&film),
            descriptor: &descriptor,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: Some(8_000_000),
                tone_map: hdr,
            }),
            audio: None,
            subtitle: None,
            encoders: &found,
            vaapi_device: &settings.vaapi_device,
            hardware_decode: way.hardware_decode,
            gpu_filters: way.gpu_filters,
            // PGPU-03 measures CUDA's filters: an HDR source keeps the
            // CPU's tone map here (PGPU-04's tone_map test has the GPU's).
            tone_map: spoolframe_media::encoders::ToneMapSetting::Cpu,
            start: None,
            output: Output::Progressive { file: out.clone() },
        };
        let mut args = ffmpeg_args(&spec).unwrap();
        let graph = args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1].clone();
        let expected = match (way.hardware_decode, way.gpu_filters) {
            (false, _) => Decode::Software,
            (true, true) if !hdr => Decode::Gpu,
            (true, _) => Decode::Hardware,
        };
        assert_eq!(decode(&spec), expected, "{}", way.name);
        assert_eq!(
            made(&spec).decoder,
            if way.hardware_decode {
                Decoder::Hardware(Hardware::Nvenc)
            } else {
                Decoder::Software
            },
            "{}",
            way.name
        );
        if way.hardware_decode {
            assert_eq!(gpu_decoder(&spec), Some(Hardware::Nvenc));
            assert!(args.windows(2).any(|w| w == ["-hwaccel", "cuda"]));
        }
        if expected == Decode::Gpu {
            assert!(graph.contains("scale_cuda"), "{graph}");
        } else {
            assert!(!graph.contains("_cuda"), "{graph}");
        }
        if given.is_some() {
            // A minute of a real film, not all of it.
            let at = args.len() - 1;
            args.splice(at..at, ["-t".to_owned(), "60".to_owned()]);
        }
        let (used, took) = run(&sandbox, &film, &dir, &args).await;
        assert!(out.metadata().unwrap().len() > 0);
        let against = cpu.first().map_or(String::new(), |software: &Duration| {
            format!(
                " ({:+.0}% against software)",
                (used.as_secs_f64() / software.as_secs_f64() - 1.0) * 100.0
            )
        });
        lines.push(format!(
            "  {:<30} {:>8.1} s CPU, {:>6.1} s wall{against}\n  {:<30} {graph}",
            way.name,
            used.as_secs_f64(),
            took.as_secs_f64(),
            ""
        ));
        cpu.push(used);
    }
    eprintln!(
        "\nPGPU-03 nvdec: {} ({}x{} {} {}-bit {}), {} to 1080p H.264 on NVENC:\n{}\n",
        film.display(),
        video.width,
        video.height,
        video.codec,
        video.bit_depth,
        if hdr {
            "HDR, tone mapped on the CPU"
        } else {
            "SDR"
        },
        if given.is_some() { "60 s" } else { "all 5 s" },
        lines.join("\n")
    );
    assert!(
        cpu[1] < cpu[0],
        "NVDEC took no less CPU than software: {cpu:?}"
    );
    if hdr {
        eprintln!("HDR source: NVIDIA tone maps on the CPU, so the third run filtered there too");
    } else {
        assert!(
            cpu[2] < cpu[1],
            "CUDA filters took no less CPU than NVDEC with CPU filters: {cpu:?}"
        );
    }
}
