//! One GPU per job (PX-28), end to end on a host with an NVIDIA and an
//! AMD GPU: real detection of both under `transcode.hardware = ["nvenc",
//! "vaapi"]`, where [`placement`] puts each job, and 20 s of it run
//! confined as the server runs it, with each GPU's peak use while it ran.
//! Ignored: needs both GPUs. On one (fulcrum's GTX 1060 + Radeon 890M):
//!
//! ```text
//! SPOOLFRAME_PICK_VAAPI=/dev/dri/renderD128 \
//! SPOOLFRAME_PICK_FILES=/a/film.mkv:/another.mkv \
//! CARGO_INCREMENTAL=0 devenv shell -- cargo test -p spoolframe-server --test it \
//!     gpu_pick -- --ignored --nocapture
//! ```
//!
//! A generated 1080p AV1 clip is always among the films: NVDEC on Pascal
//! cannot decode it, so its jobs belong on VA-API.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use spoolframe_media::Prober;
use spoolframe_media::encoders::{
    self, FfmpegCommand, Hardware, HardwarePolicy, TranscodeSettings,
};
use spoolframe_media::sandbox::{self, Jail, Limits, Mode, Sandbox, Tool};
use spoolframe_protocol::ConvertedVideo;
use spoolframe_server::encode::args::{EncodeSpec, Output, ffmpeg_args, made, placement};
use spoolframe_types::VideoCodec;

use crate::support::fresh_dir;

/// A 3-second 1080p AV1 clip, made in software.
fn av1_clip(dir: &Path) -> PathBuf {
    let film = dir.join("av1.mkv");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1920x1080:rate=24:duration=3",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libsvtav1",
            "-preset",
            "12",
        ])
        .arg(&film)
        .output()
        .expect("ffmpeg runs");
    assert!(made.status.success(), "{made:?}");
    film
}

/// The highest use, in percent, each GPU showed while `busy` was set:
/// AMD's from sysfs, NVIDIA's encoder and decoder from nvidia-smi.
#[derive(Default)]
struct Peaks {
    amd: AtomicU32,
    nvidia: AtomicU32,
}

async fn sample(peaks: Arc<Peaks>, busy: Arc<AtomicBool>) {
    while busy.load(Ordering::Relaxed) {
        if let Ok(mut cards) = tokio::fs::read_dir("/sys/class/drm").await {
            while let Ok(Some(card)) = cards.next_entry().await {
                let path = card.path().join("device/gpu_busy_percent");
                if let Ok(text) = tokio::fs::read_to_string(&path).await
                    && let Ok(percent) = text.trim().parse::<u32>()
                {
                    peaks.amd.fetch_max(percent, Ordering::Relaxed);
                }
            }
        }
        if let Ok(out) = tokio::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=utilization.encoder,utilization.decoder",
                "--format=csv,noheader,nounits",
            ])
            .output()
            .await
        {
            for percent in String::from_utf8_lossy(&out.stdout)
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter_map(|v| v.parse::<u32>().ok())
            {
                peaks.nvidia.fetch_max(percent, Ordering::Relaxed);
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an NVIDIA and an AMD GPU: run by hand on a host with both"]
async fn each_job_runs_on_the_gpu_that_does_the_most_of_it() {
    let dir = fresh_dir("gpu-pick").await;
    let vaapi = std::env::var_os("SPOOLFRAME_PICK_VAAPI")
        .map_or_else(|| PathBuf::from("/dev/dri/renderD128"), PathBuf::from);
    let sandbox = Sandbox::new(
        Mode::Required,
        Vec::new(),
        sandbox::gpu_devices(Some(&vaapi), true),
    )
    .expect("this kernel has Landlock");
    let settings = TranscodeSettings {
        hardware: HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi]),
        vaapi_device: vaapi.clone(),
        ..TranscodeSettings::default()
    };
    let ffmpeg = FfmpegCommand::new(Tool::new("ffmpeg", sandbox.clone()));
    let found = encoders::detect(&ffmpeg, &settings, &dir.join("detect")).await;
    eprintln!("detected: {}", found.summary());

    let mut films = vec![av1_clip(&dir)];
    if let Some(given) = std::env::var_os("SPOOLFRAME_PICK_FILES") {
        films.extend(std::env::split_paths(&given));
    }
    let prober = Prober::new(
        Tool::new("ffprobe", sandbox.clone()),
        Prober::DEFAULT_TIMEOUT,
    );
    for (n, film) in films.iter().enumerate() {
        let descriptor = prober.describe(film).await.unwrap();
        let stream = descriptor.video.as_ref().expect("a video stream");
        let height = stream.height;
        let mut heights = vec![height];
        if height > 1080 {
            heights.push(1080);
        }
        for (m, out_height) in heights.into_iter().enumerate() {
            let out = dir.join(format!("out-{n}-{m}.mkv"));
            let spec = EncodeSpec {
                input: film,
                sidecar: None,
                descriptor: &descriptor,
                video: Some(ConvertedVideo::Encode {
                    codec: VideoCodec::H264,
                    height: out_height,
                    max_bitrate: Some(8_000_000),
                    // As the planner asks for H.264: HDR sources only.
                    tone_map: stream.hdr.is_some(),
                }),
                audio: None,
                subtitle: None,
                encoders: &found,
                vaapi_device: &vaapi,
                hardware_decode: true,
                gpu_filters: true,
                tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
                start: None,
                output: Output::Progressive { file: out.clone() },
            };
            let place = placement(&spec).expect("H.264 encodes somewhere");
            let made = made(&spec);
            let mut args = ffmpeg_args(&spec).unwrap();
            let at = args.len() - 1;
            args.splice(at..at, ["-t".to_owned(), "20".to_owned()]);

            let jail = Jail::new(Limits::encode(4 << 30))
                .input(film)
                .writes(dir.to_path_buf())
                .gpu(place.gpu());
            let mut command = Tool::new("ffmpeg", sandbox.clone()).command(&jail).unwrap();
            command.args(&args).kill_on_drop(true);
            let peaks = Arc::new(Peaks::default());
            let busy = Arc::new(AtomicBool::new(true));
            let sampler = tokio::spawn(sample(peaks.clone(), busy.clone()));
            let before = children_cpu();
            let started = std::time::Instant::now();
            let ran = tokio::time::timeout(Duration::from_secs(600), command.output())
                .await
                .expect("ffmpeg took over ten minutes")
                .unwrap();
            let cpu = children_cpu() - before;
            let wall = started.elapsed();
            busy.store(false, Ordering::Relaxed);
            sampler.await.unwrap();

            eprintln!(
                "{} {}-bit {}p → H.264 {out_height}p: encoder {} on {:?}, decoder {:?}, \
                 tone map {:?}, run {}, 20 s in {:.1} s ({:.1}× real time), CPU {:.1} s, peak AMD {}%, NVIDIA enc/dec {}%",
                film.file_name().unwrap().to_string_lossy(),
                stream.bit_depth,
                height,
                place.encoder.name,
                place.gpu(),
                made.decoder,
                made.tone_map,
                if ran.status.success() { "ok" } else { "FAILED" },
                wall.as_secs_f64(),
                20.0 / wall.as_secs_f64(),
                cpu.as_secs_f64(),
                peaks.amd.load(Ordering::Relaxed),
                peaks.nvidia.load(Ordering::Relaxed),
            );
            assert!(
                ran.status.success(),
                "{}",
                String::from_utf8_lossy(&ran.stderr)
            );
            if n == 0 {
                // NVDEC on Pascal lacks AV1: the whole job is VA-API's.
                assert_eq!(place.encoder.hardware, Some(Hardware::Vaapi));
                assert_eq!(place.decoder, Some(Hardware::Vaapi));
            }
        }
    }
}
