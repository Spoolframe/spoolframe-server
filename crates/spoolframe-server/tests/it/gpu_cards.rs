//! Several GPUs (PGPU-05), end to end on a real host: the cards the
//! kernel lists, how each was classified, detection on each as the server
//! runs it (confined, every card of every API), and one short encode per
//! card, confined to that card's nodes. With two VA-API cards, a run on
//! one is shown to be refused the other's node. Each card that tone maps
//! with libplacebo (PGPU-04) tone maps 3 s of HDR10 to SDR with it, on
//! its own nodes, and the output must be BT.709.
//!
//! `SPOOLFRAME_GPU_CARDS_PICK=igpu` (or `all`, `best`, `dgpu`): one
//! VA-API entry with that pick, rather than every card of every API.
//!
//! Ignored: it needs GPUs, and prints what it found. On a host:
//!
//! ```text
//! CARGO_INCREMENTAL=0 devenv shell -- cargo test -p spoolframe-server --test it \
//!     gpu_cards -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use spoolframe_media::Prober;
use spoolframe_media::encoders::{Hardware, ToneMapSetting};
use spoolframe_media::gpus::{self, GpuEntry, HostRoots, Pick, Which};
use spoolframe_media::sandbox::{GpuDevices, GpuUse, Jail, Limits, Mode, Sandbox, Tool};
use spoolframe_protocol::ConvertedVideo;
use spoolframe_server::encode::args::{
    EncodeSpec, Inputs, Output, ffmpeg_args, made, placement, tone_mapper, vulkan,
};
use spoolframe_server::transcode::Transcoding;
use spoolframe_types::VideoCodec;

use crate::support::fresh_dir;

/// A 3-second 1080p H.264 clip, made in software.
fn clip(dir: &Path) -> PathBuf {
    let film = dir.join("clip.mkv");
    let made = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(["-i", "testsrc2=size=1920x1080:rate=24:duration=3"])
        .args([
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
        ])
        .arg(&film)
        .output()
        .expect("ffmpeg runs");
    assert!(made.status.success(), "{made:?}");
    film
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs GPUs: run by hand on a host with them"]
async fn each_card_is_found_classified_and_encodes_confined() {
    let dir = fresh_dir("gpu-cards").await;
    let host = gpus::host_cards(&HostRoots::default());
    eprintln!("cards on this host:");
    for card in &host {
        eprintln!(
            "  {} {:04x}:{:04x} driver {}, render {:?}, by-path {:?}, nvidia {:?}: {} ({})",
            card.pci,
            card.vendor,
            card.device_id,
            card.driver,
            card.render,
            card.by_path,
            card.nvidia,
            card.class.kind.as_str(),
            card.class.how,
        );
    }

    // As the server: no GPU node in the base sandbox, each card's given
    // to its own runs.
    let sandbox = Sandbox::new(Mode::Required, Vec::new(), GpuDevices::default())
        .expect("this kernel has Landlock");
    let entries = match std::env::var("SPOOLFRAME_GPU_CARDS_PICK") {
        Ok(word) => vec![GpuEntry {
            which: Which::Pick(Pick::parse(&word).expect("all, best, dgpu or igpu")),
            ..GpuEntry::all(Hardware::Vaapi)
        }],
        Err(_) => GpuEntry::defaults(),
    };
    let transcoding = Transcoding::new(
        Tool::new("ffmpeg", sandbox.clone()),
        entries,
        dir.join("detect"),
    )
    .on_host(host, Path::new("/dev"));
    transcoding.detect().await;
    let detected = transcoding.detected();
    eprintln!("merged: {}", detected.encoders.summary());
    assert!(!detected.gpus.is_empty(), "no GPU passed detection here");

    let film = clip(&dir);
    let prober = Prober::new(Tool::new("ffprobe", sandbox), Prober::DEFAULT_TIMEOUT);
    let descriptor = prober.describe(&film).await.unwrap();
    for (n, gpu) in detected.gpus.iter().enumerate() {
        let card = gpu.card();
        eprintln!(
            "GPU {n}: {} ({}, {}): {} ({}), capability {}; nodes {:?}",
            gpu.found.name,
            card.api.as_str(),
            card.place(),
            card.host.class.kind.as_str(),
            card.host.class.how,
            gpu.found.capability,
            card.devices(Path::new("/dev")),
        );
        let tone_maps: Vec<&str> = gpu
            .encoders()
            .gpu_tone_mappers(card.api)
            .into_iter()
            .map(|m| m.label())
            .collect();
        eprintln!(
            "  tone maps on the GPU: {}",
            if tone_maps.is_empty() {
                "none".to_owned()
            } else {
                tone_maps.join(", ")
            }
        );
        let node = gpu.vaapi_device();
        let out = dir.join(format!("out-{n}.mkv"));
        let spec = EncodeSpec {
            inputs: Inputs::film(&film),
            descriptor: &descriptor,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 720,
                max_bitrate: Some(4_000_000),
                tone_map: false,
            }),
            audio: None,
            subtitle: None,
            encoders: gpu.encoders(),
            vaapi_device: &node,
            hardware_decode: true,
            gpu_filters: false,
            tone_map: ToneMapSetting::Auto,
            start: None,
            output: Output::Progressive { file: out.clone() },
        };
        let place = placement(&spec).expect("H.264 encodes somewhere");
        let made = made(&spec);
        let args = ffmpeg_args(&spec).unwrap();
        let jail = Jail::new(Limits::encode(1 << 30))
            .input(&film)
            .writes(dir.to_path_buf())
            .gpu(place.gpu());
        let mut command = gpu.ffmpeg.command(&jail).unwrap();
        command.args(&args).kill_on_drop(true);
        let started = Instant::now();
        let ran = tokio::time::timeout(Duration::from_secs(120), command.output())
            .await
            .expect("ffmpeg took over two minutes")
            .unwrap();
        eprintln!(
            "  3 s of 1080p H.264 to 720p: encoder {} ({:?}), decoder {:?}, {} in {:.2} s",
            place.encoder.name,
            place.gpu(),
            made.decoder,
            if ran.status.success() { "ok" } else { "FAILED" },
            started.elapsed().as_secs_f64(),
        );
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
    }

    // libplacebo's tone map on each card that runs it (PGPU-04 with
    // PGPU-05): the job's run is that card's ffmpeg, its jail the card's
    // nodes alone, so libplacebo's own Vulkan device lands on it.
    let hdr = hdr_clip(&dir);
    let hdr_descriptor = prober.describe(&hdr).await.unwrap();
    for (n, gpu) in detected.gpus.iter().enumerate() {
        if gpu.encoders().libplacebo(gpu.card().api).is_none() {
            continue;
        }
        let node = gpu.vaapi_device();
        let out = dir.join(format!("hdr-{n}.mkv"));
        let spec = EncodeSpec {
            inputs: Inputs::film(&hdr),
            descriptor: &hdr_descriptor,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 720,
                max_bitrate: Some(4_000_000),
                tone_map: true,
            }),
            audio: None,
            subtitle: None,
            encoders: gpu.encoders(),
            vaapi_device: &node,
            hardware_decode: true,
            gpu_filters: true,
            tone_map: ToneMapSetting::Libplacebo,
            start: None,
            output: Output::Progressive { file: out.clone() },
        };
        let place = placement(&spec).expect("H.264 encodes somewhere");
        let ran_as = tone_mapper(&spec).expect("tone mapped");
        let jail = Jail::new(Limits::encode(1 << 30))
            .input(&hdr)
            .writes(dir.to_path_buf())
            .gpu(place.gpu())
            .vulkan(vulkan(&spec));
        let mut command = gpu.ffmpeg.command(&jail).unwrap();
        command.args(ffmpeg_args(&spec).unwrap()).kill_on_drop(true);
        let started = Instant::now();
        let ran = tokio::time::timeout(Duration::from_secs(120), command.output())
            .await
            .expect("ffmpeg took over two minutes")
            .unwrap();
        let colour = colour(&out);
        eprintln!(
            "GPU {n} ({}): 3 s of 1080p HDR10 to 720p SDR: {} with {} (Vulkan on {}), {} in {:.2} s, {}/{}",
            gpu.found.name,
            place.encoder.name,
            ran_as.label(),
            jail_vulkan(&spec),
            if ran.status.success() { "ok" } else { "FAILED" },
            started.elapsed().as_secs_f64(),
            colour.0,
            colour.1,
        );
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(colour, ("bt709".to_owned(), "bt709".to_owned()));
    }

    // A run on one VA-API card is refused the other's render node.
    let vaapi: Vec<_> = detected
        .gpus
        .iter()
        .filter(|g| {
            g.card().host.render.is_some()
                && g.card().api != spoolframe_media::encoders::Hardware::Nvenc
        })
        .collect();
    if let [first, second, ..] = vaapi.as_slice() {
        let other = second.vaapi_device();
        let open = |on: &spoolframe_server::transcode::Gpu| {
            let mut command = on
                .ffmpeg
                .command(
                    &Jail::new(Limits::PROBE)
                        .writes(dir.to_path_buf())
                        .gpu(GpuUse::Vaapi),
                )
                .unwrap();
            command
                .args(["-hide_banner", "-loglevel", "error", "-init_hw_device"])
                .arg(format!("vaapi=va:{}", other.display()))
                .args(["-f", "lavfi", "-i", "nullsrc=d=0.1", "-f", "null", "-"]);
            command.output()
        };
        // Its own card's run opens it: the refusal below is the sandbox's.
        let own = open(second).await.unwrap();
        assert!(
            own.status.success(),
            "{}",
            String::from_utf8_lossy(&own.stderr)
        );
        let ran = open(first).await.unwrap();
        eprintln!(
            "a run on {} opening {}: {} ({}); on its own card: opened",
            first.vaapi_device().display(),
            other.display(),
            if ran.status.success() {
                "OPENED"
            } else {
                "refused"
            },
            String::from_utf8_lossy(&ran.stderr)
                .lines()
                .last()
                .unwrap_or(""),
        );
        assert!(!ran.status.success(), "a run opened another card's node");
    }
}

/// The GPU a run of `spec` names to Mesa's device-select layer, for the
/// line: `1002:744c`, or `-`.
fn jail_vulkan(spec: &EncodeSpec<'_>) -> String {
    vulkan(spec)
        .and_then(|v| v.pci_ids)
        .map_or_else(|| "-".to_owned(), |(v, d)| format!("{v:04x}:{d:04x}"))
}

/// 3 s of 1080p HDR10 HEVC, 10-bit, PQ BT.2020, made in software.
fn hdr_clip(dir: &Path) -> PathBuf {
    let film = dir.join("hdr10.mkv");
    let made = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args([
            "-i",
            "testsrc2=size=1920x1080:rate=24:duration=3,format=yuv420p10le,\
             setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
        ])
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
