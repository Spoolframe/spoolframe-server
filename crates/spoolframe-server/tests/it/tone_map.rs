//! The HDR tone map on the GPU (PGPU-04), end to end: real detection,
//! confined as the server confines it, then 20 s of an HDR10 film to
//! 1080p H.264 with each tone map this host runs, each confined as an
//! encode job is: the CPU's (on the first GPU family, as today), VA-API's
//! own (`tonemap_vaapi`) where the driver has it, and libplacebo on each
//! family where its test passed. Each run prints × real time and the CPU
//! it took, and its output must be SDR BT.709. Ignored: it needs a GPU.
//!
//! ```text
//! CARGO_INCREMENTAL=0 devenv shell -- cargo test -p spoolframe-server --test it \
//!     tone_map -- --ignored --nocapture
//! ```
//!
//! - `SPOOLFRAME_TONEMAP_FILE=/a/hdr-film.mkv`: 20 s of a real film
//!   rather than the generated 4K HDR10 clip.
//! - `SPOOLFRAME_TONEMAP_VAAPI=/dev/dri/renderD129`: the VA-API render
//!   node (renderD128 if left out).
//! - `SPOOLFRAME_TONEMAP_SHARED_ON_AMD=1`: as
//!   `experimental.libplacebo_shared_on_amd`.
//! - `SPOOLFRAME_TONEMAP_SANDBOX=off`: unconfined, to tell a sandbox
//!   refusal from a driver one.
//! - `SPOOLFRAME_TONEMAP_HARDWARE=nvenc` (or `vaapi`): that family only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use spoolframe_media::Prober;
use spoolframe_media::encoders::{
    self, Encoders, FfmpegCommand, Hardware, HardwarePolicy, ToneMapSetting, TranscodeSettings,
};
use spoolframe_media::sandbox::{self, Jail, Limits, Mode, Sandbox, Tool};
use spoolframe_protocol::ConvertedVideo;
use spoolframe_server::encode::args::{
    EncodeSpec, Output, ffmpeg_args, placement, tone_mapper, vulkan,
};
use spoolframe_types::VideoCodec;

use crate::support::fresh_dir;

/// How long of the film each run encodes.
const SECONDS: u32 = 20;

/// 20 s of 4K HDR10 HEVC, 10-bit, PQ BT.2020 with its mastering metadata.
fn clip(dir: &Path) -> PathBuf {
    let film = dir.join("hdr10-4k.mkv");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!(
            "testsrc2=size=3840x2160:rate=24:duration={SECONDS},format=yuv420p10le,\
             setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc"
        ))
        .args([
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error:hdr10=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:\
             master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):\
             max-cll=1000,400",
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

/// `found`, with only `family`'s encoders and decoders (and software's):
/// so the job runs on that family's GPU.
fn only(found: &Encoders, family: Hardware) -> Encoders {
    Encoders {
        video: found
            .video
            .iter()
            .filter(|e| e.hardware.is_none_or(|h| h == family))
            .cloned()
            .collect(),
        hardware_decoders: found
            .hardware_decoders
            .iter()
            .filter(|d| d.hardware == family)
            .cloned()
            .collect(),
        ..found.clone()
    }
}

/// One way to tone map.
struct Method {
    name: String,
    family: Hardware,
    setting: ToneMapSetting,
}

/// The ways this host tone maps, of what detection found.
fn methods(found: &Encoders) -> Vec<Method> {
    let families: Vec<Hardware> = [Hardware::Vaapi, Hardware::Nvenc]
        .into_iter()
        .filter(|family| {
            found
                .video
                .iter()
                .any(|e| e.codec == VideoCodec::H264 && e.hardware == Some(*family))
        })
        .collect();
    let mut methods = Vec::new();
    if let Some(first) = families.first() {
        methods.push(Method {
            name: format!("CPU tone map, {} encode", first.as_str()),
            family: *first,
            setting: ToneMapSetting::Cpu,
        });
    }
    for family in families {
        if family == Hardware::Vaapi && found.gpu_tonemap {
            methods.push(Method {
                name: "VA-API (tonemap_vaapi)".into(),
                family,
                setting: ToneMapSetting::Gpu,
            });
        }
        if found.libplacebo(family).is_some() {
            methods.push(Method {
                name: format!("libplacebo on {}", family.as_str()),
                family,
                setting: ToneMapSetting::Libplacebo,
            });
        }
    }
    methods
}

/// The output's colour, as ffprobe reads it: (transfer, primaries).
fn colour(out: &Path) -> (String, String) {
    let probed = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_transfer,color_primaries",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(out)
        .output()
        .expect("ffprobe runs");
    let text = String::from_utf8_lossy(&probed.stdout).into_owned();
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .unwrap_or("")
            .to_owned()
    };
    (field("color_transfer"), field("color_primaries"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a GPU: run by hand on one"]
async fn each_tone_map_this_host_runs_makes_sdr_bt709() {
    let dir = fresh_dir("tone-map").await;
    let vaapi = std::env::var_os("SPOOLFRAME_TONEMAP_VAAPI")
        .map_or_else(|| PathBuf::from("/dev/dri/renderD128"), PathBuf::from);
    let mode = match std::env::var("SPOOLFRAME_TONEMAP_SANDBOX").as_deref() {
        Ok("off") => Mode::Off,
        _ => Mode::Required,
    };
    let nvidia = Path::new("/dev/nvidiactl").exists();
    let sandbox = Sandbox::new(mode, Vec::new(), sandbox::gpu_devices(Some(&vaapi), nvidia))
        .expect("this kernel has Landlock");
    let hardware = match std::env::var("SPOOLFRAME_TONEMAP_HARDWARE").as_deref() {
        Ok("nvenc") => HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
        Ok("vaapi") => HardwarePolicy::Ordered(vec![Hardware::Vaapi]),
        _ => HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi]),
    };
    let settings = TranscodeSettings {
        hardware,
        vaapi_device: vaapi.clone(),
        libplacebo_shared_on_amd: std::env::var_os("SPOOLFRAME_TONEMAP_SHARED_ON_AMD").is_some(),
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

    let given = std::env::var_os("SPOOLFRAME_TONEMAP_FILE").map(PathBuf::from);
    let film = given.clone().unwrap_or_else(|| clip(&dir));
    let prober = Prober::new(
        Tool::new("ffprobe", sandbox.clone()),
        Prober::DEFAULT_TIMEOUT,
    );
    let descriptor = prober.describe(&film).await.unwrap();
    let video = descriptor.video.clone().expect("a video stream");
    assert!(
        video.hdr.is_some(),
        "{} is not HDR: nothing to tone map",
        film.display()
    );

    let methods = methods(&found);
    assert!(!methods.is_empty(), "no GPU encodes H.264 here");
    let mut lines = Vec::new();
    let mut wrong = Vec::new();
    for (i, method) in methods.iter().enumerate() {
        let encoders = only(&found, method.family);
        let out = dir.join(format!("out-{i}.mkv"));
        let spec = EncodeSpec {
            input: &film,
            sidecar: None,
            descriptor: &descriptor,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: Some(8_000_000),
                tone_map: true,
            }),
            audio: None,
            subtitle: None,
            encoders: &encoders,
            vaapi_device: &vaapi,
            hardware_decode: true,
            gpu_filters: true,
            tone_map: method.setting,
            start: None,
            output: Output::Progressive { file: out.clone() },
        };
        let place = placement(&spec).expect("an encoder");
        assert_eq!(
            place.encoder.hardware,
            Some(method.family),
            "{}",
            method.name
        );
        let ran_as = tone_mapper(&spec).expect("tone mapped");
        let mut args = ffmpeg_args(&spec).unwrap();
        let graph = args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1].clone();
        let at = args.len() - 1;
        args.splice(at..at, ["-t".to_owned(), SECONDS.to_string()]);
        let jail = Jail::new(Limits::encode(4 << 30))
            .input(&film)
            .writes(&*dir)
            .gpu(place.gpu())
            .vulkan(vulkan(&spec));
        let mut command = Tool::new("ffmpeg", sandbox.clone()).command(&jail).unwrap();
        command.args(&args).kill_on_drop(true);
        let before = children_cpu();
        let started = Instant::now();
        let output = tokio::time::timeout(Duration::from_secs(600), command.output())
            .await
            .expect("ffmpeg took over ten minutes")
            .unwrap();
        let (cpu, wall) = (children_cpu() - before, started.elapsed());
        if !output.status.success() {
            lines.push(format!(
                "  {:<34} FAILED ({}): {}\n  {:<34} {graph}",
                method.name,
                ran_as.label(),
                String::from_utf8_lossy(&output.stderr).trim(),
                ""
            ));
            wrong.push(format!("{} failed", method.name));
            continue;
        }
        let (transfer, primaries) = colour(&out);
        if (transfer.as_str(), primaries.as_str()) != ("bt709", "bt709") {
            wrong.push(format!(
                "{}: color_transfer={transfer} color_primaries={primaries}",
                method.name
            ));
        }
        lines.push(format!(
            "  {:<34} {:>5.1}× real time, {:>6.1} s CPU, {:>5.1} s wall [{}; {transfer}/{primaries}]\n  {:<34} {graph}",
            method.name,
            f64::from(SECONDS) / wall.as_secs_f64(),
            cpu.as_secs_f64(),
            wall.as_secs_f64(),
            ran_as.label(),
            ""
        ));
    }
    eprintln!(
        "\nPGPU-04 tone map: {} ({}x{} {} {}-bit), {SECONDS} s to 1080p H.264, {}:\n{}\n",
        film.display(),
        video.width,
        video.height,
        video.codec,
        video.bit_depth,
        if mode == Mode::Off {
            "unconfined"
        } else {
            "confined"
        },
        lines.join("\n")
    );
    assert!(wrong.is_empty(), "{wrong:?}");
}
