//! Which encoders work on this machine (P2-02).
//!
//! An encoder compiled into ffmpeg is not a usable one: VAAPI needs a
//! render node and a driver that supports the codec, NVENC the NVIDIA
//! driver. So every hardware and software video encoder ffmpeg lists is
//! tried with a one-second encode of a test pattern, and only the ones that
//! succeed are kept. On the dev machine (Radeon RX 7900 and a Raphael iGPU)
//! `av1_vaapi` lists, but encodes only on one of the two render nodes, and
//! `h264_nvenc` lists with no NVIDIA driver at all.
//!
//! Audio encoders need no device; ffmpeg's list is enough for them.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use spoolframe_types::{AudioCodec, VideoCodec};

use crate::sandbox::{Jail, Limits, Tool};

/// A family of hardware video encoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hardware {
    /// VA-API: Intel and AMD GPUs on Linux.
    Vaapi,
    /// NVIDIA.
    Nvenc,
    /// Intel Quick Sync through oneVPL. nixpkgs' ffmpeg-headless does not
    /// build it; Intel GPUs encode through VA-API instead.
    Qsv,
}

impl Hardware {
    pub const ALL: [Hardware; 3] = [Hardware::Vaapi, Hardware::Nvenc, Hardware::Qsv];

    fn suffix(self) -> &'static str {
        match self {
            Hardware::Vaapi => "vaapi",
            Hardware::Nvenc => "nvenc",
            Hardware::Qsv => "qsv",
        }
    }
}

/// Which hardware encoders to try, and in which order
/// (`transcode { hardware = …; }`).
///
/// For each codec the first family whose test encode works is preferred.
/// So `[Nvenc, Vaapi]` on a host with a GTX 1060 and a Radeon encodes
/// H.264 and HEVC on the 1060 and AV1, which Pascal's NVENC lacks, on the
/// Radeon; software stays the last resort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardwarePolicy {
    /// Every family, in [`Hardware::ALL`]'s order.
    Auto,
    /// These families, in this order.
    Ordered(Vec<Hardware>),
    /// Software only.
    None,
}

impl HardwarePolicy {
    /// The families to try, in order: every one for `Auto`.
    pub fn families(&self) -> Vec<Hardware> {
        match self {
            HardwarePolicy::Auto => Hardware::ALL.to_vec(),
            HardwarePolicy::Ordered(order) => order.clone(),
            HardwarePolicy::None => Vec::new(),
        }
    }

    /// The families named explicitly, whose failure is worth a warning.
    pub fn named(&self) -> &[Hardware] {
        match self {
            HardwarePolicy::Ordered(order) => order,
            HardwarePolicy::Auto | HardwarePolicy::None => &[],
        }
    }
}

/// How to detect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeSettings {
    pub hardware: HardwarePolicy,
    /// The render node VA-API encodes on.
    pub vaapi_device: PathBuf,
}

impl Default for TranscodeSettings {
    fn default() -> Self {
        Self {
            hardware: HardwarePolicy::Auto,
            vaapi_device: PathBuf::from("/dev/dri/renderD128"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VideoEncoder {
    pub codec: VideoCodec,
    /// ffmpeg's name for it, e.g. `hevc_vaapi`.
    pub name: String,
    /// `None` for software.
    pub hardware: Option<Hardware>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioEncoder {
    pub codec: AudioCodec,
    pub name: String,
}

/// A hardware decoder that worked in a test decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HardwareDecoder {
    pub codec: VideoCodec,
    pub hardware: Hardware,
}

/// What this machine can encode, in order of preference: hardware first,
/// within a codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Encoders {
    pub video: Vec<VideoEncoder>,
    pub audio: Vec<AudioEncoder>,
    pub hardware_decoders: Vec<HardwareDecoder>,
    /// Hardware the settings named that did not work, for the log.
    #[serde(skip)]
    pub unusable: Vec<Hardware>,
}

impl Encoders {
    /// What the planner assumes before detection has run: ffmpeg's built-in
    /// AAC and libx264, which every ffmpeg build Spoolframe ships has.
    pub fn software_baseline() -> Self {
        Self {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "libx264".into(),
                hardware: None,
            }],
            audio: vec![AudioEncoder {
                codec: AudioCodec::Aac,
                name: "aac".into(),
            }],
            hardware_decoders: Vec::new(),
            unusable: Vec::new(),
        }
    }

    /// The preferred encoder for `codec`.
    pub fn video_encoder(&self, codec: VideoCodec) -> Option<&VideoEncoder> {
        self.video.iter().find(|e| e.codec == codec)
    }

    pub fn audio_encoder(&self, codec: AudioCodec) -> Option<&AudioEncoder> {
        self.audio.iter().find(|e| e.codec == codec)
    }

    /// One line for the log: `video: hevc_vaapi, h264_vaapi, libx264, …`.
    pub fn summary(&self) -> String {
        let names = |v: Vec<&str>| {
            if v.is_empty() {
                "none".to_owned()
            } else {
                v.join(", ")
            }
        };
        format!(
            "video: {}; audio: {}; hardware decoders: {}",
            names(self.video.iter().map(|e| e.name.as_str()).collect()),
            names(self.audio.iter().map(|e| e.name.as_str()).collect()),
            names(
                self.hardware_decoders
                    .iter()
                    .map(|d| d.codec.as_str())
                    .collect()
            ),
        )
    }
}

/// Video codecs Spoolframe encodes to, most widely playable first; each
/// with its software encoder.
const VIDEO: [(VideoCodec, &str, &str); 3] = [
    (VideoCodec::H264, "h264", "libx264"),
    (VideoCodec::Hevc, "hevc", "libx265"),
    (VideoCodec::Av1, "av1", "libsvtav1"),
];

const AUDIO: [(AudioCodec, &str); 4] = [
    (AudioCodec::Aac, "aac"),
    (AudioCodec::Eac3, "eac3"),
    (AudioCodec::Ac3, "ac3"),
    (AudioCodec::Opus, "libopus"),
];

/// Runs ffmpeg for detection. A trait so the logic is tested without one.
#[async_trait]
pub trait Ffmpeg: Send + Sync {
    /// The output of `ffmpeg -encoders`.
    async fn encoders(&self) -> Result<String, String>;
    /// Whether ffmpeg with `args` exits successfully. What it reads and
    /// writes is in `scratch`.
    async fn succeeds(&self, args: &[String], scratch: &Path) -> bool;
}

/// The names in `ffmpeg -encoders` output.
fn listed(output: &str) -> Vec<&str> {
    output
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect()
}

/// A one-second encode of a test pattern with `encoder`.
fn encode_args(
    encoder: &str,
    hardware: Option<Hardware>,
    settings: &TranscodeSettings,
) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error"]
        .map(String::from)
        .to_vec();
    if hardware == Some(Hardware::Vaapi) {
        args.extend([
            "-vaapi_device".into(),
            settings.vaapi_device.display().to_string(),
        ]);
    }
    args.extend(
        [
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30",
            "-t",
            "1",
        ]
        .map(String::from),
    );
    if hardware == Some(Hardware::Vaapi) {
        args.extend(["-vf".into(), "format=nv12,hwupload".into()]);
    }
    args.extend([
        "-c:v".into(),
        encoder.into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    args
}

/// A one-second hardware decode of `sample`.
fn decode_args(hardware: Hardware, sample: &Path, settings: &TranscodeSettings) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error"]
        .map(String::from)
        .to_vec();
    // Asking for frames to stay in hardware memory makes a decode that
    // would fall back to software fail instead.
    match hardware {
        Hardware::Vaapi => args.extend([
            "-hwaccel".into(),
            "vaapi".into(),
            "-hwaccel_device".into(),
            settings.vaapi_device.display().to_string(),
            "-hwaccel_output_format".into(),
            "vaapi".into(),
        ]),
        Hardware::Nvenc => {
            args.extend(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"].map(String::from))
        }
        Hardware::Qsv => {
            args.extend(["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"].map(String::from))
        }
    }
    args.extend([
        "-i".into(),
        sample.display().to_string(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    args
}

/// Find what works. `scratch` is a directory for the test samples the
/// hardware decoders are tried on.
pub async fn detect(ffmpeg: &dyn Ffmpeg, settings: &TranscodeSettings, scratch: &Path) -> Encoders {
    // Every run is confined to it, so it is there before the first.
    let scratch_made = std::fs::create_dir_all(scratch).is_ok();
    let list = match ffmpeg.encoders().await {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(
                error = e,
                "cannot list ffmpeg's encoders; assuming software only"
            );
            return Encoders::software_baseline();
        }
    };
    let listed = listed(&list);
    let families = settings.hardware.families();

    let mut video = Vec::new();
    for (codec, prefix, software) in VIDEO {
        let mut candidates: Vec<(String, Option<Hardware>)> = families
            .iter()
            .map(|h| (format!("{prefix}_{}", h.suffix()), Some(*h)))
            .collect();
        candidates.push((software.to_owned(), None));
        for (name, hardware) in candidates {
            if !listed.contains(&name.as_str()) {
                continue;
            }
            if ffmpeg
                .succeeds(&encode_args(&name, hardware, settings), scratch)
                .await
            {
                video.push(VideoEncoder {
                    codec,
                    name,
                    hardware,
                });
            }
        }
    }

    let audio = AUDIO
        .iter()
        .filter(|(_, name)| listed.contains(name))
        .map(|(codec, name)| AudioEncoder {
            codec: *codec,
            name: (*name).to_owned(),
        })
        .collect();

    let mut hardware_decoders = Vec::new();
    if !families.is_empty() && scratch_made {
        for (codec, _, software) in VIDEO {
            let sample = scratch.join(format!("sample-{}.mkv", codec.as_str()));
            let made = listed.contains(&software)
                && ffmpeg
                    .succeeds(&sample_args(software, &sample), scratch)
                    .await;
            if !made {
                continue;
            }
            for hardware in &families {
                if ffmpeg
                    .succeeds(&decode_args(*hardware, &sample, settings), scratch)
                    .await
                {
                    hardware_decoders.push(HardwareDecoder {
                        codec,
                        hardware: *hardware,
                    });
                }
            }
            let _ = std::fs::remove_file(&sample);
        }
    }

    let unusable = families
        .iter()
        .copied()
        .filter(|h| !video.iter().any(|e: &VideoEncoder| e.hardware == Some(*h)))
        .collect();
    Encoders {
        video,
        audio,
        hardware_decoders,
        unusable,
    }
}

/// A one-second software encode to `out`, for the decode tests.
fn sample_args(encoder: &str, out: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"]
        .map(String::from)
        .to_vec();
    args.extend(
        [
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30",
            "-t",
            "1",
            "-pix_fmt",
            "yuv420p",
        ]
        .map(String::from),
    );
    args.extend(["-c:v".into(), encoder.into(), out.display().to_string()]);
    args
}

/// The real ffmpeg.
#[derive(Debug, Clone)]
pub struct FfmpegCommand {
    pub tool: Tool,
    /// Per run. A wedged driver must not hold up start-up for long.
    pub timeout: Duration,
}

impl FfmpegCommand {
    pub fn new(tool: Tool) -> Self {
        Self {
            tool,
            timeout: Duration::from_secs(15),
        }
    }
}

/// What a test encode or decode may use: an encode's memory (less, and an
/// encoder that works would be left out), the GPU, and its scratch
/// directory.
fn test_jail(scratch: Option<&Path>) -> Jail {
    let jail = Jail::new(Limits::encode(64 << 20)).gpu(true);
    match scratch {
        Some(dir) => jail.writes(dir),
        None => jail,
    }
}

#[async_trait]
impl Ffmpeg for FfmpegCommand {
    async fn encoders(&self) -> Result<String, String> {
        let path = self.tool.program().display();
        let mut command = self
            .tool
            .command(&test_jail(None))
            .map_err(|e| format!("cannot run {path}: {e}"))?;
        let output = tokio::time::timeout(
            self.timeout,
            command
                .args(["-hide_banner", "-encoders"])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| format!("{path} -encoders timed out"))?
        .map_err(|e| format!("cannot run {path}: {e}"))?;
        if !output.status.success() {
            return Err(format!("{path} -encoders failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn succeeds(&self, args: &[String], scratch: &Path) -> bool {
        let Ok(mut command) = self.tool.command(&test_jail(Some(scratch))) else {
            return false;
        };
        let run = command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status();
        matches!(
            tokio::time::timeout(self.timeout, run).await,
            Ok(Ok(status)) if status.success()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// ffmpeg as recorded on the dev machine: its encoder list, and the
    /// encoder names whose test encode (or decode, by hwaccel) succeeds.
    struct Recorded {
        works: Vec<&'static str>,
        runs: Mutex<Vec<Vec<String>>>,
    }

    impl Recorded {
        fn new(works: &[&'static str]) -> Self {
            Self {
                works: works.to_vec(),
                runs: Mutex::default(),
            }
        }
    }

    #[async_trait]
    impl Ffmpeg for Recorded {
        async fn encoders(&self) -> Result<String, String> {
            Ok(std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/ffmpeg-encoders.txt"
            ))
            .unwrap())
        }

        async fn succeeds(&self, args: &[String], _scratch: &Path) -> bool {
            self.runs.lock().unwrap().push(args.to_vec());
            // A sample encode writes a file; a decode names its hwaccel.
            let name = args
                .iter()
                .position(|a| a == "-c:v")
                .map(|i| args[i + 1].clone())
                .or_else(|| {
                    args.iter()
                        .position(|a| a == "-hwaccel")
                        .map(|i| format!("decode_{}", args[i + 1]))
                })
                .unwrap();
            self.works.iter().any(|w| *w == name)
        }
    }

    /// A directory of the test's own, removed when dropped.
    async fn scratch(name: &str) -> spoolframe_test_dir::TestDir {
        spoolframe_test_dir::TestDir::new(&format!("spoolframe-encoders-{name}")).await
    }

    fn names(e: &Encoders) -> Vec<&str> {
        e.video.iter().map(|v| v.name.as_str()).collect()
    }

    #[tokio::test]
    async fn working_hardware_comes_before_software_and_listed_but_broken_is_dropped() {
        // The dev machine: VA-API works for H.264 and HEVC, AV1 only in
        // software here; NVENC is listed but has no driver.
        let ffmpeg = Recorded::new(&[
            "h264_vaapi",
            "hevc_vaapi",
            "libx264",
            "libx265",
            "libsvtav1",
            "decode_vaapi",
        ]);
        let e = detect(
            &ffmpeg,
            &TranscodeSettings::default(),
            scratch("auto").await.as_ref(),
        )
        .await;
        assert_eq!(
            names(&e),
            [
                "h264_vaapi",
                "libx264",
                "hevc_vaapi",
                "libx265",
                "libsvtav1"
            ]
        );
        assert_eq!(
            e.video_encoder(VideoCodec::Hevc).unwrap().name,
            "hevc_vaapi"
        );
        assert_eq!(e.video_encoder(VideoCodec::Av1).unwrap().name, "libsvtav1");
        assert_eq!(e.unusable, [Hardware::Nvenc, Hardware::Qsv]);
        assert!(
            e.hardware_decoders
                .iter()
                .all(|d| d.hardware == Hardware::Vaapi)
        );
        assert_eq!(e.hardware_decoders.len(), 3);
    }

    #[tokio::test]
    async fn qsv_is_not_tried_when_ffmpeg_does_not_list_it() {
        let ffmpeg = Recorded::new(&["libx264"]);
        detect(
            &ffmpeg,
            &TranscodeSettings::default(),
            scratch("qsv").await.as_ref(),
        )
        .await;
        let runs = ffmpeg.runs.lock().unwrap();
        assert!(runs.iter().all(|r| !r.iter().any(|a| a.contains("_qsv"))));
    }

    #[tokio::test]
    async fn a_failed_hardware_probe_falls_back_to_software() {
        let ffmpeg = Recorded::new(&["libx264", "libx265", "aac"]);
        let e = detect(
            &ffmpeg,
            &TranscodeSettings::default(),
            scratch("fallback").await.as_ref(),
        )
        .await;
        assert_eq!(e.video_encoder(VideoCodec::H264).unwrap().hardware, None);
        assert!(e.hardware_decoders.is_empty());
    }

    #[tokio::test]
    async fn hardware_none_tries_only_software() {
        let ffmpeg = Recorded::new(&["h264_vaapi", "libx264"]);
        let settings = TranscodeSettings {
            hardware: HardwarePolicy::None,
            ..TranscodeSettings::default()
        };
        let e = detect(&ffmpeg, &settings, scratch("none").await.as_ref()).await;
        assert_eq!(names(&e), ["libx264"]);
        assert!(e.unusable.is_empty());
        let runs = ffmpeg.runs.lock().unwrap();
        assert!(runs.iter().all(|r| !r.iter().any(|a| a.contains("vaapi"))));
    }

    #[tokio::test]
    async fn naming_one_family_tries_only_it_and_reports_it_unusable() {
        let ffmpeg = Recorded::new(&["h264_vaapi", "libx264"]);
        let settings = TranscodeSettings {
            hardware: HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
            ..TranscodeSettings::default()
        };
        let e = detect(&ffmpeg, &settings, scratch("nvenc").await.as_ref()).await;
        assert_eq!(names(&e), ["libx264"]);
        assert_eq!(e.unusable, [Hardware::Nvenc]);
    }

    #[tokio::test]
    async fn audio_encoders_come_from_the_list() {
        let e = detect(
            &Recorded::new(&[]),
            &TranscodeSettings::default(),
            scratch("audio").await.as_ref(),
        )
        .await;
        let audio: Vec<_> = e.audio.iter().map(|a| a.codec).collect();
        assert_eq!(
            audio,
            [
                AudioCodec::Aac,
                AudioCodec::Eac3,
                AudioCodec::Ac3,
                AudioCodec::Opus
            ]
        );
    }

    #[tokio::test]
    async fn an_ordered_list_prefers_its_first_family_per_codec_and_falls_through() {
        // A GTX 1060 (NVENC: H.264 and HEVC, no AV1) beside a Radeon
        // (VA-API: all three).
        let ffmpeg = Recorded::new(&[
            "h264_nvenc",
            "hevc_nvenc",
            "h264_vaapi",
            "hevc_vaapi",
            "av1_vaapi",
            "libx264",
        ]);
        let settings = TranscodeSettings {
            hardware: HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi]),
            ..TranscodeSettings::default()
        };
        let e = detect(&ffmpeg, &settings, scratch("ordered").await.as_ref()).await;
        assert_eq!(
            e.video_encoder(VideoCodec::H264).unwrap().name,
            "h264_nvenc"
        );
        assert_eq!(
            e.video_encoder(VideoCodec::Hevc).unwrap().name,
            "hevc_nvenc"
        );
        assert_eq!(e.video_encoder(VideoCodec::Av1).unwrap().name, "av1_vaapi");
        // The others stay as fallbacks, in order.
        let h264: Vec<_> = e
            .video
            .iter()
            .filter(|v| v.codec == VideoCodec::H264)
            .map(|v| v.name.as_str())
            .collect();
        assert_eq!(h264, ["h264_nvenc", "h264_vaapi", "libx264"]);
    }

    #[test]
    fn vaapi_encodes_upload_frames_to_the_render_node() {
        let args = encode_args(
            "hevc_vaapi",
            Some(Hardware::Vaapi),
            &TranscodeSettings::default(),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-vaapi_device /dev/dri/renderD128"));
        assert!(joined.contains("-vf format=nv12,hwupload"));
    }
}
