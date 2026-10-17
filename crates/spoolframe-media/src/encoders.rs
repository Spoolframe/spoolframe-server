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

use crate::sandbox::{GpuUse, Jail, Limits, Tool, Vulkan};

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

    /// The GPU a run on this family uses: QSV goes through VA-API's
    /// render node, NVENC and NVDEC through NVIDIA's nodes.
    pub fn gpu(self) -> GpuUse {
        match self {
            Hardware::Vaapi | Hardware::Qsv => GpuUse::Vaapi,
            Hardware::Nvenc => GpuUse::Nvidia,
        }
    }

    /// Its name in `transcode.gpus`' `api` and in ffmpeg's encoder names:
    /// `vaapi`, `nvenc`, `qsv`.
    pub fn as_str(self) -> &'static str {
        match self {
            Hardware::Vaapi => "vaapi",
            Hardware::Nvenc => "nvenc",
            Hardware::Qsv => "qsv",
        }
    }
}

/// Which hardware encoders to try, and in which order: detection tests
/// each card with its own API alone (`gpus::Card::settings`).
///
/// Detection lists each codec's working encoders in this order, and the
/// server runs each video encode on the family that does the most of it
/// (`encode::args::placement`, PGPU-02), this order deciding between
/// equals. So `[Nvenc, Vaapi]` on a host with a GTX 1060 and a Radeon
/// encodes AV1, which Pascal's NVENC lacks, on the Radeon, and an AV1
/// film to H.264 there too, Pascal's NVDEC not decoding AV1; software
/// stays the last resort.
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

    /// The families named explicitly: what they lack is logged at info,
    /// and one with no working encoder at all is a warning.
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
    /// `transcode.tone_map` as the server started: only for the warning
    /// at detection when a forced method runs on no GPU here. Jobs read
    /// the live setting.
    pub tone_map: ToneMapSetting,
    /// `experimental.libplacebo_shared_on_amd` (PGPU-04): libplacebo on the
    /// VA-API GPU tries ffmpeg's Vulkan device first.
    pub libplacebo_shared_on_amd: bool,
}

impl Default for TranscodeSettings {
    fn default() -> Self {
        Self {
            hardware: HardwarePolicy::Auto,
            vaapi_device: PathBuf::from("/dev/dri/renderD128"),
            tone_map: ToneMapSetting::Auto,
            libplacebo_shared_on_amd: false,
        }
    }
}

/// `transcode.tone_map` (PGPU-04): how an HDR source is tone mapped to SDR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToneMapSetting {
    /// The job's GPU's own tone map (`tonemap_vaapi`) while
    /// `transcode.gpu_filters` is on, else libplacebo (whatever
    /// `gpu_filters` says), else the CPU.
    #[default]
    Auto,
    /// The CPU's (`zscale` and `tonemap=hable`).
    Cpu,
    /// The GPU driver's own: `tonemap_vaapi` (Intel's VA-API driver).
    Gpu,
    /// libplacebo, on the job's GPU through Vulkan.
    Libplacebo,
}

impl ToneMapSetting {
    /// Its word in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Libplacebo => "libplacebo",
        }
    }
}

impl std::str::FromStr for ToneMapSetting {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "gpu" => Ok(Self::Gpu),
            "libplacebo" => Ok(Self::Libplacebo),
            other => Err(format!(
                "{other:?}: one of \"auto\", \"cpu\", \"gpu\" or \"libplacebo\""
            )),
        }
    }
}

/// Where libplacebo's Vulkan device comes from (PGPU-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VulkanDevice {
    /// ffmpeg's: the frames stay on the GPU from the decode to the tone
    /// map. NVIDIA always; AMD only with
    /// `experimental.libplacebo_shared_on_amd`, RADV failing it today.
    Shared,
    /// libplacebo's own, on the job's GPU: the frames go to it through
    /// system memory and come back the same way.
    Own,
}

/// libplacebo's tone map works on a family's GPU, with this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Libplacebo {
    pub hardware: Hardware,
    pub device: VulkanDevice,
}

/// What tone maps a job's HDR video to SDR (PGPU-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToneMapper {
    Cpu,
    /// `tonemap_vaapi`.
    Vaapi,
    Libplacebo(VulkanDevice),
}

impl ToneMapper {
    /// For the log and Server › Playback: `CPU`, `VA-API`,
    /// `libplacebo (shared)`, `libplacebo (own)`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Vaapi => "VA-API",
            Self::Libplacebo(VulkanDevice::Shared) => "libplacebo (shared)",
            Self::Libplacebo(VulkanDevice::Own) => "libplacebo (own)",
        }
    }

    /// Whether it runs on the GPU.
    pub fn on_gpu(self) -> bool {
        self != Self::Cpu
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
    /// The family whose hwaccel decoded it: NVENC's is NVDEC, through
    /// CUDA.
    pub hardware: Hardware,
    /// The test sample's bits per sample: 8, or 10 for the 10-bit
    /// samples (HDR sources are 10-bit).
    pub bit_depth: u32,
}

impl HardwareDecoder {
    /// What a family lacks when this decode fails: `hevc 10-bit decode`.
    fn lacked(&self) -> String {
        if self.bit_depth > 8 {
            format!("{} {}-bit decode", self.codec.as_str(), self.bit_depth)
        } else {
            format!("{} decode", self.codec.as_str())
        }
    }

    /// For the log and the admin API: `hevc (vaapi)`,
    /// `hevc 10-bit (nvdec)`.
    pub fn label(&self) -> String {
        let api = match self.hardware {
            Hardware::Vaapi => "vaapi",
            Hardware::Nvenc => "nvdec",
            Hardware::Qsv => "qsv",
        };
        if self.bit_depth > 8 {
            format!("{} {}-bit ({api})", self.codec.as_str(), self.bit_depth)
        } else {
            format!("{} ({api})", self.codec.as_str())
        }
    }
}

/// What this machine can encode, in order of preference: hardware first,
/// within a codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Encoders {
    pub video: Vec<VideoEncoder>,
    pub audio: Vec<AudioEncoder>,
    pub hardware_decoders: Vec<HardwareDecoder>,
    /// VA-API tone maps HDR to SDR on the GPU (`tonemap_vaapi`).
    pub gpu_tonemap: bool,
    /// VA-API lays pictures over video on the GPU (`overlay_vaapi`).
    pub gpu_overlay: bool,
    /// NVIDIA keeps NVDEC's frames on the GPU and scales them there
    /// (`scale_cuda`, PGPU-03): converting a 10-bit picture to 8 bits too.
    pub cuda_scale: bool,
    /// NVIDIA lays pictures over video on the GPU (`overlay_cuda`).
    pub cuda_overlay: bool,
    /// The families whose GPU tone maps with libplacebo, and how (PGPU-04).
    pub libplacebo: Vec<Libplacebo>,
    /// The GPU encoders of [`Encoders::video`] that took a 10-bit test
    /// frame (PGPU-06): what an encode that keeps a film's HDR may run on.
    /// HEVC and AV1 only; H.264 carries no HDR.
    pub ten_bit: Vec<VideoEncoder>,
    /// Hardware the settings named that did not work, for the log.
    #[serde(skip)]
    pub unusable: Vec<Hardware>,
    /// Each family's tests that failed, for the log (PGPU-02).
    #[serde(skip)]
    pub failed: Vec<FailedTest>,
}

/// A test of a GPU family that failed at detection: what it says the
/// family lacks, and why, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTest {
    pub hardware: Hardware,
    /// What was tried: the encoder (`av1_nvenc`), the decode
    /// (`decode hevc 10-bit (nvdec)`) or the filter (`tonemap_vaapi`).
    pub test: String,
    /// What the family lacks for it: `av1 encode`, `hevc 10-bit decode`,
    /// `tone map`.
    pub lacks: String,
    /// What ffmpeg said last.
    pub stderr: String,
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

    /// Whether `encoder` took a 10-bit test frame (PGPU-06).
    pub fn encodes_ten_bit(&self, encoder: &VideoEncoder) -> bool {
        self.ten_bit.iter().any(|e| e.name == encoder.name)
    }

    /// How libplacebo tone maps on `hardware`'s GPU, if it does.
    pub fn libplacebo(&self, hardware: Hardware) -> Option<VulkanDevice> {
        self.libplacebo
            .iter()
            .find(|l| l.hardware == hardware)
            .map(|l| l.device)
    }

    /// The tone maps `hardware`'s GPU runs, in `transcode.tone_map =
    /// "auto"`'s order: its own (`tonemap_vaapi`), then libplacebo.
    pub fn gpu_tone_mappers(&self, hardware: Hardware) -> Vec<ToneMapper> {
        let own = (hardware == Hardware::Vaapi && self.gpu_tonemap).then_some(ToneMapper::Vaapi);
        let placebo = self.libplacebo(hardware).map(ToneMapper::Libplacebo);
        own.into_iter().chain(placebo).collect()
    }

    /// The filters `hardware`'s GPU runs that detection found: VA-API's
    /// `tone map` and `overlay`, NVIDIA's `scale` and `overlay`.
    pub fn gpu_filters(&self, hardware: Hardware) -> Vec<&'static str> {
        let filters: &[(bool, &'static str)] = match hardware {
            Hardware::Vaapi => &[
                (self.gpu_tonemap, "tone map"),
                (self.gpu_overlay, "overlay"),
            ],
            Hardware::Nvenc => &[(self.cuda_scale, "scale"), (self.cuda_overlay, "overlay")],
            Hardware::Qsv => &[],
        };
        filters
            .iter()
            .filter_map(|(found, name)| found.then_some(*name))
            .collect()
    }

    /// Whether `setting` forces a method no GPU here runs: what the
    /// server warns of once detection is done.
    pub fn forced_nowhere(&self, setting: ToneMapSetting) -> bool {
        match setting {
            ToneMapSetting::Auto | ToneMapSetting::Cpu => false,
            ToneMapSetting::Gpu => !self.gpu_tonemap,
            ToneMapSetting::Libplacebo => self.libplacebo.is_empty(),
        }
    }

    /// What `hardware`'s failed tests say it lacks, in the order tried:
    /// `["av1 encode", "av1 decode"]`.
    pub fn lacks(&self, hardware: Hardware) -> Vec<&str> {
        self.failed
            .iter()
            .filter(|f| f.hardware == hardware)
            .map(|f| f.lacks.as_str())
            .collect()
    }

    /// Whether `hardware` decoded `codec` in a test decode, of any depth.
    pub fn decodes(&self, codec: VideoCodec, hardware: Hardware) -> bool {
        self.hardware_decoders
            .iter()
            .any(|d| d.codec == codec && d.hardware == hardware)
    }

    /// Whether `hardware` decoded a sample of `codec` with at least
    /// `bit_depth` bits: a 10-bit source needs a 10-bit test decode, a
    /// GPU that decodes 8-bit HEVC not always decoding 10-bit.
    pub fn decodes_depth(&self, codec: VideoCodec, bit_depth: u32, hardware: Hardware) -> bool {
        self.hardware_decoders
            .iter()
            .any(|d| d.codec == codec && d.hardware == hardware && d.bit_depth >= bit_depth)
    }

    /// The preferred encoder for `codec`.
    pub fn video_encoder(&self, codec: VideoCodec) -> Option<&VideoEncoder> {
        self.video.iter().find(|e| e.codec == codec)
    }

    pub fn audio_encoder(&self, codec: AudioCodec) -> Option<&AudioEncoder> {
        self.audio.iter().find(|e| e.codec == codec)
    }

    /// One line for the log: `video: hevc_vaapi, h264_vaapi, libx264, …;
    /// … hardware decoders: h264 (vaapi), hevc 10-bit (nvdec), …`.
    pub fn summary(&self) -> String {
        let names = |v: Vec<&str>| {
            if v.is_empty() {
                "none".to_owned()
            } else {
                v.join(", ")
            }
        };
        // Per family: `vaapi: tone map, overlay; nvenc: scale, overlay`.
        let families: Vec<String> = [Hardware::Vaapi, Hardware::Nvenc]
            .into_iter()
            .map(|family| (family, self.gpu_filters(family)))
            .filter(|(_, filters)| !filters.is_empty())
            .map(|(family, filters)| format!("{}: {}", family.as_str(), filters.join(", ")))
            .collect();
        let decoders: Vec<String> = self
            .hardware_decoders
            .iter()
            .map(HardwareDecoder::label)
            .collect();
        // Per family: `vaapi: VA-API, libplacebo (own); nvenc: libplacebo
        // (shared)`.
        let tone_maps: Vec<String> = [Hardware::Vaapi, Hardware::Nvenc]
            .into_iter()
            .filter_map(|family| {
                let found = self.gpu_tone_mappers(family);
                (!found.is_empty()).then(|| {
                    let labels: Vec<&str> = found.into_iter().map(ToneMapper::label).collect();
                    format!("{}: {}", family.as_str(), labels.join(", "))
                })
            })
            .collect();
        format!(
            "video: {}; audio: {}; hardware decoders: {}; GPU filters: {}; GPU tone maps: {}",
            names(self.video.iter().map(|e| e.name.as_str()).collect()),
            names(self.audio.iter().map(|e| e.name.as_str()).collect()),
            names(decoders.iter().map(String::as_str).collect()),
            if families.is_empty() {
                "none".to_owned()
            } else {
                families.join("; ")
            },
            if tone_maps.is_empty() {
                "none".to_owned()
            } else {
                tone_maps.join("; ")
            },
        )
    }
}

/// Video codecs Spoolframe encodes to, most widely playable first; each
/// with its software encoder, H.264 alone having one (PGPU-06): x265 or
/// SVT-AV1 on a 4K film take several cores each, where libx264 is cheap,
/// so a software encode is always H.264 and the others are not tried.
const VIDEO: [(VideoCodec, &str, Option<&str>); 3] = [
    (VideoCodec::H264, "h264", Some("libx264")),
    (VideoCodec::Hevc, "hevc", None),
    (VideoCodec::Av1, "av1", None),
];

const AUDIO: [(AudioCodec, &str); 4] = [
    (AudioCodec::Aac, "aac"),
    (AudioCodec::Eac3, "eac3"),
    (AudioCodec::Ac3, "ac3"),
    (AudioCodec::Opus, "libopus"),
];

/// The GPU a detection run may use, and whether through Vulkan too
/// (libplacebo's tests, PGPU-04).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestGpu {
    pub gpu: GpuUse,
    pub vulkan: Option<Vulkan>,
}

impl From<GpuUse> for TestGpu {
    fn from(gpu: GpuUse) -> Self {
        Self { gpu, vulkan: None }
    }
}

/// Runs ffmpeg for detection. A trait so the logic is tested without one.
#[async_trait]
pub trait Ffmpeg: Send + Sync {
    /// The output of `ffmpeg -encoders`.
    async fn encoders(&self) -> Result<String, String>;
    /// Whether ffmpeg with `args` exits successfully: if not, why, which
    /// is the last of what it wrote to stderr when it ran. What it reads
    /// and writes is in `scratch`; `gpu` is the GPU it may use.
    async fn succeeds(&self, args: &[String], scratch: &Path, gpu: TestGpu) -> Result<(), String>;
    /// What ffmpeg with `args` wrote to stderr, however it ended: a
    /// card's name, as its driver tells it at `-v verbose` (PGPU-05).
    /// `None` where it could not run; stand-ins say nothing.
    async fn stderr(&self, _args: &[String], _scratch: &Path, _gpu: GpuUse) -> Option<String> {
        None
    }
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

/// A one-second encode of a test pattern with `encoder`, 8-bit.
fn encode_args(
    encoder: &str,
    hardware: Option<Hardware>,
    settings: &TranscodeSettings,
) -> Vec<String> {
    depth_encode_args(encoder, hardware, settings, false)
}

/// A one-second encode of a test pattern with `encoder`, 10-bit
/// (`p010`) with `ten_bit`: what an encode that keeps HDR gives it
/// (PGPU-06). A GPU that encodes 8-bit HEVC does not always encode
/// Main 10.
fn depth_encode_args(
    encoder: &str,
    hardware: Option<Hardware>,
    settings: &TranscodeSettings,
    ten_bit: bool,
) -> Vec<String> {
    let format = if ten_bit { "p010" } else { "nv12" };
    let mut args = quiet();
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
        args.extend(["-vf".into(), format!("format={format},hwupload")]);
    } else if ten_bit {
        args.extend(["-pix_fmt".into(), "p010le".into()]);
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

/// ffmpeg's options to decode the next input on `hardware`, VA-API on
/// `vaapi_device`: previews, test decodes and transcodes all decode this
/// way.
///
/// Without `keep_frames` the decoded frames come back to memory, where
/// any software filter takes them; and a decoder that cannot take the
/// stream (10-bit H.264, 4:2:2) falls back to software inside ffmpeg,
/// with no error. With it they stay in GPU memory for GPU filters and
/// encoders, and that same stream fails instead: software frames are not
/// what the filters after it take.
pub fn hwaccel_args(hardware: Hardware, vaapi_device: &Path, keep_frames: bool) -> Vec<String> {
    let (name, format) = match hardware {
        Hardware::Vaapi => ("vaapi", "vaapi"),
        Hardware::Nvenc => ("cuda", "cuda"),
        Hardware::Qsv => ("qsv", "qsv"),
    };
    let mut args = vec!["-hwaccel".to_owned(), name.to_owned()];
    if hardware == Hardware::Vaapi {
        args.extend([
            "-hwaccel_device".to_owned(),
            vaapi_device.display().to_string(),
        ]);
    }
    if keep_frames {
        args.extend(["-hwaccel_output_format".to_owned(), format.to_owned()]);
    }
    args
}

/// A one-second hardware decode of `sample`.
fn decode_args(hardware: Hardware, sample: &Path, settings: &TranscodeSettings) -> Vec<String> {
    let mut args = quiet();
    // Asking for frames to stay in hardware memory makes a decode that
    // would fall back to software fail instead.
    args.extend(hwaccel_args(hardware, &settings.vaapi_device, true));
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
    // Every run is confined to it, so it is there before the first. Without
    // it no hardware decoder is tried, and the log says why (review
    // 2026-10-11 L61): the summary would say only "hardware decoders: none".
    let scratch_made = match std::fs::create_dir_all(scratch) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                scratch = %scratch.display(),
                error = %e,
                "cannot make the encoder test directory; no hardware decoder is tried"
            );
            false
        }
    };
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

    let mut failures = Vec::new();
    let mut video = Vec::new();
    for (codec, prefix, software) in VIDEO {
        let mut candidates: Vec<(String, Option<Hardware>)> = families
            .iter()
            .map(|h| (format!("{prefix}_{}", h.as_str()), Some(*h)))
            .collect();
        candidates.extend(software.map(|name| (name.to_owned(), None)));
        for (name, hardware) in candidates {
            if !listed.contains(&name.as_str()) {
                continue;
            }
            let gpu = hardware.map_or(GpuUse::None, Hardware::gpu);
            match ffmpeg
                .succeeds(&encode_args(&name, hardware, settings), scratch, gpu.into())
                .await
            {
                Ok(()) => video.push(VideoEncoder {
                    codec,
                    name,
                    hardware,
                }),
                Err(why) => failed(
                    &mut failures,
                    hardware,
                    &name,
                    &format!("{} encode", codec.as_str()),
                    why,
                ),
            }
        }
    }

    // The GPU encoders that keep 10 bits (PGPU-06), for an encode that
    // keeps a film's HDR.
    let mut ten_bit = Vec::new();
    for encoder in video
        .iter()
        .filter(|e| e.hardware.is_some() && e.codec != VideoCodec::H264)
    {
        let hardware = encoder.hardware;
        let gpu = hardware.map_or(GpuUse::None, Hardware::gpu);
        match ffmpeg
            .succeeds(
                &depth_encode_args(&encoder.name, hardware, settings, true),
                scratch,
                gpu.into(),
            )
            .await
        {
            Ok(()) => ten_bit.push(encoder.clone()),
            Err(why) => failed(
                &mut failures,
                hardware,
                &format!("{} 10-bit", encoder.name),
                &format!("{} 10-bit encode", encoder.codec.as_str()),
                why,
            ),
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
    // The H.264 sample, where NVDEC decoded it: the CUDA filter tests
    // decode it again, as a job decodes its film.
    let mut cuda_sample = None;
    if scratch_made {
        for sample in &SAMPLES {
            let tried = sample.tried_on(&families, &video);
            if tried.is_empty() || !listed.contains(&sample.encoder) {
                continue;
            }
            let file = scratch.join(format!(
                "sample-{}-{}.mkv",
                sample.codec.as_str(),
                sample.bit_depth
            ));
            if let Err(why) = ffmpeg
                .succeeds(&sample.args(&file), scratch, GpuUse::None.into())
                .await
            {
                failed(&mut failures, None, sample.encoder, "", why);
                continue;
            }
            for hardware in tried {
                let decoder = HardwareDecoder {
                    codec: sample.codec,
                    hardware,
                    bit_depth: sample.bit_depth,
                };
                match ffmpeg
                    .succeeds(
                        &decode_args(hardware, &file, settings),
                        scratch,
                        hardware.gpu().into(),
                    )
                    .await
                {
                    Ok(()) => hardware_decoders.push(decoder),
                    Err(why) => failed(
                        &mut failures,
                        Some(hardware),
                        &format!("decode {}", decoder.label()),
                        &decoder.lacked(),
                        why,
                    ),
                }
            }
            let nvdec_h264 = sample.codec == VideoCodec::H264
                && sample.bit_depth == 8
                && hardware_decoders
                    .iter()
                    .any(|d| d.codec == VideoCodec::H264 && d.hardware == Hardware::Nvenc);
            if nvdec_h264 {
                cuda_sample = Some(file);
            } else {
                let _ = std::fs::remove_file(&file);
            }
        }
    }

    let unusable = families
        .iter()
        .copied()
        .filter(|h| !video.iter().any(|e: &VideoEncoder| e.hardware == Some(*h)))
        .collect();
    // GPU filters feed a VA-API encoder; drivers differ in which they
    // have (Mesa: neither; Intel's iHD: both).
    let vaapi = video.iter().any(|e| e.hardware == Some(Hardware::Vaapi));
    let gpu_tonemap = vaapi
        && gpu_filter(
            ffmpeg,
            scratch,
            &mut failures,
            (Hardware::Vaapi, "tonemap_vaapi", "tone map"),
            tonemap_args(settings),
            None,
        )
        .await;
    let gpu_overlay = vaapi
        && gpu_filter(
            ffmpeg,
            scratch,
            &mut failures,
            (Hardware::Vaapi, "overlay_vaapi", "overlay"),
            overlay_args(settings),
            None,
        )
        .await;
    // CUDA filters (PGPU-03) feed NVENC from NVDEC's frames: tried where
    // both work, on the sample NVDEC decoded.
    let h264_nvenc = video
        .iter()
        .any(|e| e.codec == VideoCodec::H264 && e.hardware == Some(Hardware::Nvenc));
    let (mut cuda_scale, mut cuda_overlay) = (false, false);
    if let Some(sample) = cuda_sample {
        if h264_nvenc {
            cuda_scale = gpu_filter(
                ffmpeg,
                scratch,
                &mut failures,
                (Hardware::Nvenc, "scale_cuda", "scale"),
                cuda_scale_args(&sample),
                None,
            )
            .await;
            cuda_overlay = gpu_filter(
                ffmpeg,
                scratch,
                &mut failures,
                (Hardware::Nvenc, "overlay_cuda", "overlay"),
                cuda_overlay_args(&sample),
                None,
            )
            .await;
        }
        let _ = std::fs::remove_file(&sample);
    }
    // libplacebo's tone map (PGPU-04), on each family whose H.264 encoder
    // works: what its test encodes to.
    let h264 = |family: Hardware| {
        video
            .iter()
            .any(|e| e.codec == VideoCodec::H264 && e.hardware == Some(family))
    };
    let mut libplacebo = Vec::new();
    if h264(Hardware::Vaapi)
        && let Some(device) = vaapi_libplacebo(ffmpeg, scratch, settings, &mut failures).await
    {
        libplacebo.push(Libplacebo {
            hardware: Hardware::Vaapi,
            device,
        });
    }
    if h264(Hardware::Nvenc) && nvidia_libplacebo(ffmpeg, scratch, &mut failures).await {
        libplacebo.push(Libplacebo {
            hardware: Hardware::Nvenc,
            device: VulkanDevice::Shared,
        });
    }
    let found = Encoders {
        video,
        audio,
        hardware_decoders,
        gpu_tonemap,
        gpu_overlay,
        cuda_scale,
        cuda_overlay,
        libplacebo,
        ten_bit,
        unusable,
        failed: failures,
    };
    if found.forced_nowhere(settings.tone_map) {
        tracing::warn!(
            tone_map = settings.tone_map.as_str(),
            "transcode.tone_map forces a tone map no GPU here runs; HDR is tone mapped on the CPU"
        );
    }
    found
}

/// What lacking libplacebo means, for the start's `… lacks:` line.
const LACKS_LIBPLACEBO: &str = "libplacebo tone map";

/// How libplacebo tone maps on the VA-API GPU, if it does: on ffmpeg's
/// Vulkan device first with `experimental.libplacebo_shared_on_amd`, on
/// its own if that fails (said at info: RADV fails it today) or the
/// switch is off.
async fn vaapi_libplacebo(
    ffmpeg: &dyn Ffmpeg,
    scratch: &Path,
    settings: &TranscodeSettings,
    failures: &mut Vec<FailedTest>,
) -> Option<VulkanDevice> {
    let gpu = TestGpu {
        gpu: GpuUse::Vaapi,
        vulkan: Some(Vulkan::on(&settings.vaapi_device)),
    };
    if settings.libplacebo_shared_on_amd {
        let shared = libplacebo_test(settings, VulkanDevice::Shared);
        match ffmpeg.succeeds(&shared, scratch, gpu.clone()).await {
            Ok(()) => return Some(VulkanDevice::Shared),
            Err(why) => {
                tracing::debug!(stderr = why, "libplacebo on ffmpeg's Vulkan device failed");
                tracing::info!(
                    "experimental.libplacebo_shared_on_amd: libplacebo cannot use ffmpeg's \
                     Vulkan device on the VA-API GPU; it makes its own"
                );
            }
        }
    }
    gpu_filter(
        ffmpeg,
        scratch,
        failures,
        (Hardware::Vaapi, "libplacebo (own)", LACKS_LIBPLACEBO),
        libplacebo_test(settings, VulkanDevice::Own),
        gpu.vulkan,
    )
    .await
    .then_some(VulkanDevice::Own)
}

/// Whether libplacebo tone maps on the NVIDIA GPU, on ffmpeg's Vulkan
/// device.
async fn nvidia_libplacebo(
    ffmpeg: &dyn Ffmpeg,
    scratch: &Path,
    failures: &mut Vec<FailedTest>,
) -> bool {
    gpu_filter(
        ffmpeg,
        scratch,
        failures,
        (Hardware::Nvenc, "libplacebo (shared)", LACKS_LIBPLACEBO),
        nvidia_libplacebo_args(),
        Some(Vulkan::default()),
    )
    .await
}

/// Whether a GPU filter test, `args`, works on `hardware`'s GPU; noted
/// if not. `test` is the filter's name, `lacks` what lacking it means;
/// `vulkan` the Vulkan device libplacebo's tests use.
async fn gpu_filter(
    ffmpeg: &dyn Ffmpeg,
    scratch: &Path,
    failures: &mut Vec<FailedTest>,
    (hardware, test, lacks): (Hardware, &str, &str),
    args: Vec<String>,
    vulkan: Option<Vulkan>,
) -> bool {
    let gpu = TestGpu {
        gpu: hardware.gpu(),
        vulkan,
    };
    match ffmpeg.succeeds(&args, scratch, gpu).await {
        Ok(()) => true,
        Err(why) => {
            failed(failures, Some(hardware), test, lacks, why);
            false
        }
    }
}

/// Logs a test that failed at debug, with what ffmpeg said last, and
/// keeps a GPU family's in `failures` for the summary the server logs
/// after detection (`transcode`): one line per family tested, rather
/// than a warning per codec it lacks.
fn failed(
    failures: &mut Vec<FailedTest>,
    hardware: Option<Hardware>,
    test: &str,
    lacks: &str,
    why: String,
) {
    tracing::debug!(
        ?hardware,
        test,
        stderr = why,
        "a test encode or decode failed"
    );
    if let Some(hardware) = hardware {
        failures.push(FailedTest {
            hardware,
            test: test.to_owned(),
            lacks: lacks.to_owned(),
            stderr: why,
        });
    }
}

/// HDR (PQ or HLG) to SDR BT.709 on the GPU, in 8 bits: the transcodes'
/// tone map when the GPU does it.
pub const TONEMAP_VAAPI: &str = "tonemap_vaapi=format=nv12:t=bt709:m=bt709:p=bt709";

/// The start of every probe run: no banner, errors only.
fn quiet() -> Vec<String> {
    ["-hide_banner", "-loglevel", "error"]
        .map(String::from)
        .to_vec()
}

/// The start of a one-second VA-API filter test on the render node.
fn filter_test(settings: &TranscodeSettings) -> Vec<String> {
    let mut args = quiet();
    args.extend([
        "-init_hw_device".into(),
        format!("vaapi=va:{}", settings.vaapi_device.display()),
        "-filter_hw_device".into(),
        "va".into(),
    ]);
    args
}

/// A second of HDR10 test pattern, 10-bit PQ BT.2020: what the tone map
/// tests tone map.
const HDR10_PATTERN: &str = "testsrc2=size=1280x720:rate=30:duration=1,format=p010le,\
     setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc";

/// libplacebo's HDR to SDR BT.709 tone map in 8 bits (PGPU-04), scaled to
/// `height` lines and an even width, as the CPU's `scale=-2` scales, if
/// given: BT.2390, mpv's default, and the measured one.
pub fn libplacebo_filter(height: Option<u32>) -> String {
    let size = height.map_or_else(String::new, |h| format!("w=-2:h={h}:"));
    format!(
        "libplacebo={size}tonemapping=bt.2390:colorspace=bt709:color_primaries=bt709:\
         color_trc=bt709:range=tv:format=nv12"
    )
}

/// libplacebo making HDR10 of a Dolby Vision profile 5 picture (PGPU-06):
/// it applies the stream's RPU (`apply_dolbyvision`, on by default) and
/// writes BT.2020 PQ in 10 bits, scaled as [`libplacebo_filter`] scales.
pub fn libplacebo_hdr10_filter(height: Option<u32>) -> String {
    let size = height.map_or_else(String::new, |h| format!("w=-2:h={h}:"));
    format!(
        "libplacebo={size}apply_dolbyvision=1:colorspace=bt2020nc:color_primaries=bt2020:\
         color_trc=smpte2084:range=tv:format=p010"
    )
}

/// ffmpeg's NVIDIA Vulkan device (PGPU-04), by name: the first whose name
/// has `NVIDIA` in it. By index it could be another vendor's GPU, or
/// Mesa's software one.
pub const NVIDIA_VULKAN: &str = "vulkan=vk:NVIDIA";

/// A second of HDR10 test pattern tone mapped by libplacebo on the
/// VA-API GPU and encoded there, as a job does: with its own device, the
/// frames from memory and back, uploaded for `h264_vaapi`; shared, on a
/// Vulkan device derived from the VA-API one, the frames mapped from
/// VA-API to it, tone mapped, downloaded and uploaded to VA-API again.
fn libplacebo_test(settings: &TranscodeSettings, device: VulkanDevice) -> Vec<String> {
    let mut args = filter_test(settings);
    let graph = match device {
        VulkanDevice::Own => format!("{},hwupload", libplacebo_filter(Some(360))),
        VulkanDevice::Shared => {
            // The filters' device is the Vulkan one, derived from VA-API's,
            // which `derive_device=vaapi` finds again for the encoder.
            args.truncate(args.len() - 2);
            args.extend(
                ["-init_hw_device", "vulkan=vk@va", "-filter_hw_device", "vk"].map(String::from),
            );
            format!(
                "hwupload=derive_device=vaapi,hwmap=derive_device=vulkan,{},\
                 hwdownload,format=nv12,hwupload=derive_device=vaapi",
                libplacebo_filter(Some(360))
            )
        }
    };
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        HDR10_PATTERN.into(),
    ]);
    args.extend(["-vf".into(), graph]);
    args.extend(["-c:v", "h264_vaapi", "-f", "null", "-"].map(String::from));
    args
}

/// A second of HDR10 test pattern uploaded to the NVIDIA Vulkan device,
/// tone mapped by libplacebo there, downloaded and encoded on NVENC: the
/// measured path, but for the decode.
fn nvidia_libplacebo_args() -> Vec<String> {
    let mut args = quiet();
    args.extend(
        [
            "-init_hw_device",
            NVIDIA_VULKAN,
            "-filter_hw_device",
            "vk",
            "-f",
            "lavfi",
            "-i",
            HDR10_PATTERN,
        ]
        .map(String::from),
    );
    args.extend([
        "-vf".into(),
        format!(
            "hwupload,{},hwdownload,format=nv12",
            libplacebo_filter(Some(360))
        ),
    ]);
    nvenc_to_null(&mut args);
    args
}

/// A second of HDR10 test pattern tone mapped on the GPU.
fn tonemap_args(settings: &TranscodeSettings) -> Vec<String> {
    let mut args = filter_test(settings);
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        HDR10_PATTERN.into(),
        "-vf".into(),
        format!("hwupload,{TONEMAP_VAAPI}"),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    args
}

/// A second of half-transparent pictures laid over a test pattern on the
/// GPU, as burned-in subtitles are.
fn overlay_args(settings: &TranscodeSettings) -> Vec<String> {
    let mut args = filter_test(settings);
    args.extend(
        [
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30:duration=1,format=nv12",
            "-f",
            "lavfi",
            "-i",
            "color=c=white@0.5:size=640x80:rate=30:duration=1,format=rgba",
            "-filter_complex",
            "[0:v]hwupload[video];[1:v]hwupload[pictures];[video][pictures]overlay_vaapi=y=600[v]",
            "-map",
            "[v]",
            "-f",
            "null",
            "-",
        ]
        .map(String::from),
    );
    args
}

/// The CUDA device a job's decode and its uploaded pictures share, by
/// name: `-hwaccel cuda` and `hwupload_cuda` would each make a CUDA
/// context of their own, and `overlay_cuda` takes frames of one.
pub fn cuda_device_args() -> Vec<String> {
    [
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
    ]
    .map(String::from)
    .to_vec()
}

/// The end of a CUDA filter test: H.264 on NVENC, from the frames on
/// the GPU, written nowhere.
fn nvenc_to_null(args: &mut Vec<String>) {
    args.extend(["-c:v", "h264_nvenc", "-f", "null", "-"].map(String::from));
}

/// The H.264 sample decoded on NVDEC, its frames kept on the GPU, scaled
/// there to 360 lines and an even width, and encoded on NVENC: what a
/// job scaling on CUDA runs.
fn cuda_scale_args(sample: &Path) -> Vec<String> {
    let mut args = quiet();
    args.extend(hwaccel_args(Hardware::Nvenc, Path::new(""), true));
    args.extend([
        "-i".into(),
        sample.display().to_string(),
        "-vf".into(),
        "scale_cuda=w=-2:h=360:format=nv12".into(),
    ]);
    nvenc_to_null(&mut args);
    args
}

/// The H.264 sample decoded on NVDEC with half-transparent pictures
/// uploaded to the same device and laid over it, as burned-in subtitles
/// are: `overlay_cuda` lays `yuva420p` over `yuv420p` only.
fn cuda_overlay_args(sample: &Path) -> Vec<String> {
    let mut args = quiet();
    args.extend(cuda_device_args());
    args.extend(["-i".into(), sample.display().to_string()]);
    args.extend(
        [
            "-f",
            "lavfi",
            "-i",
            "color=c=white@0.5:size=640x80:rate=30:duration=1,format=yuva420p",
            "-filter_complex",
            "[1:v]hwupload[pictures];[0:v]scale_cuda=format=yuv420p[video];\
             [video][pictures]overlay_cuda=y=600[v]",
            "-map",
            "[v]",
        ]
        .map(String::from),
    );
    nvenc_to_null(&mut args);
    args
}

/// A sample the hardware decoders are tried on: a second of test pattern
/// encoded in software.
struct Sample {
    codec: VideoCodec,
    bit_depth: u32,
    /// The software encoder that makes it.
    encoder: &'static str,
    pixel_format: &'static str,
    /// Options that keep a slow encoder within a second or two.
    options: &'static [&'static str],
    /// Tried on every family, or on NVDEC alone (`tried_on`).
    every_family: bool,
}

/// The decode tests' samples. The 8-bit H.264, HEVC and AV1 ones are
/// tried on every family, as before PGPU-01. NVDEC also takes 10-bit HEVC
/// and AV1 (HDR sources; Pascal has no AV1 at all) and VP9: its decodes
/// are trusted only as far as its tests went (`decodes_depth`), where
/// VA-API's are settled by `gpu_decodes` in the server and a GPU decode
/// failure's rerun in software.
const SAMPLES: [Sample; 6] = [
    Sample {
        codec: VideoCodec::H264,
        bit_depth: 8,
        encoder: "libx264",
        pixel_format: "yuv420p",
        options: &[],
        every_family: true,
    },
    Sample {
        codec: VideoCodec::Hevc,
        bit_depth: 8,
        encoder: "libx265",
        pixel_format: "yuv420p",
        options: &[],
        every_family: true,
    },
    Sample {
        codec: VideoCodec::Hevc,
        bit_depth: 10,
        encoder: "libx265",
        pixel_format: "yuv420p10le",
        options: &["-preset", "ultrafast"],
        every_family: false,
    },
    Sample {
        codec: VideoCodec::Av1,
        bit_depth: 8,
        encoder: "libsvtav1",
        pixel_format: "yuv420p",
        options: &[],
        every_family: true,
    },
    Sample {
        codec: VideoCodec::Av1,
        bit_depth: 10,
        encoder: "libsvtav1",
        pixel_format: "yuv420p10le",
        options: &["-preset", "12"],
        every_family: false,
    },
    Sample {
        codec: VideoCodec::Vp9,
        bit_depth: 8,
        encoder: "libvpx-vp9",
        pixel_format: "yuv420p",
        options: &["-deadline", "realtime", "-cpu-used", "8"],
        every_family: false,
    },
];

impl Sample {
    /// The families to try this sample on, of `families`. NVDEC decodes
    /// only for an NVENC encode, so NVDEC's own samples are made only
    /// where an NVENC encoder works: a host without one spends no time
    /// on them.
    fn tried_on(&self, families: &[Hardware], video: &[VideoEncoder]) -> Vec<Hardware> {
        if self.every_family {
            return families.to_vec();
        }
        let nvenc = video.iter().any(|e| e.hardware == Some(Hardware::Nvenc));
        families
            .iter()
            .copied()
            .filter(|h| *h == Hardware::Nvenc && nvenc)
            .collect()
    }

    /// A one-second software encode to `out`.
    fn args(&self, out: &Path) -> Vec<String> {
        let mut args = quiet();
        args.push("-y".into());
        args.extend(
            [
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1280x720:rate=30",
                "-t",
                "1",
                "-pix_fmt",
                self.pixel_format,
                "-c:v",
                self.encoder,
            ]
            .map(String::from),
        );
        args.extend(self.options.iter().map(|o| (*o).to_owned()));
        args.push(out.display().to_string());
        args
    }
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
/// encoder that works would be left out), the GPU it tests, and its
/// scratch directory.
fn test_jail(scratch: Option<&Path>, gpu: TestGpu) -> Jail {
    let jail = Jail::new(Limits::encode(64 << 20))
        .gpu(gpu.gpu)
        .vulkan(gpu.vulkan);
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
            .command(&test_jail(None, GpuUse::None.into()))
            .map_err(|e| format!("cannot run {path}: {e}"))?;
        let output = tokio::time::timeout(
            self.timeout,
            crate::capped::output(
                command
                    .args(["-hide_banner", "-encoders"])
                    .stdin(Stdio::null())
                    .kill_on_drop(true),
                crate::capped::KEPT,
            ),
        )
        .await
        .map_err(|_| format!("{path} -encoders timed out"))?
        .map_err(|e| format!("cannot run {path}: {e}"))?;
        if !output.status.success() {
            return Err(format!("{path} -encoders failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn succeeds(&self, args: &[String], scratch: &Path, gpu: TestGpu) -> Result<(), String> {
        let path = self.tool.program().display();
        let mut command = self
            .tool
            .command(&test_jail(Some(scratch), gpu))
            .map_err(|e| format!("cannot run {path}: {e}"))?;
        let run = crate::capped::output(
            command.args(args).stdin(Stdio::null()).kill_on_drop(true),
            crate::capped::KEPT,
        );
        let output = tokio::time::timeout(self.timeout, run)
            .await
            .map_err(|_| format!("{path} took over {:?}", self.timeout))?
            .map_err(|e| format!("cannot run {path}: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(last_words(&output.stderr, output.status))
    }

    async fn stderr(&self, args: &[String], scratch: &Path, gpu: GpuUse) -> Option<String> {
        let mut command = self
            .tool
            .command(&test_jail(Some(scratch), gpu.into()))
            .ok()?;
        let run = crate::capped::output(
            command.args(args).stdin(Stdio::null()).kill_on_drop(true),
            crate::capped::KEPT,
        );
        let output = tokio::time::timeout(self.timeout, run).await.ok()?.ok()?;
        Some(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// What a failed ffmpeg said last: its last two lines on stderr, each cut
/// short, after how it ended.
fn last_words(stderr: &[u8], status: std::process::ExitStatus) -> String {
    const LINES: usize = 2;
    const LONGEST: usize = 300;
    let stderr = String::from_utf8_lossy(stderr);
    let mut lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .take(LINES)
        .map(|line| match line.char_indices().nth(LONGEST) {
            Some((at, _)) => &line[..at],
            None => line,
        })
        .collect();
    if lines.is_empty() {
        return format!("{status}, saying nothing");
    }
    lines.reverse();
    format!("{status}: {}", lines.join(" / "))
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

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

        async fn succeeds(
            &self,
            args: &[String],
            _scratch: &Path,
            _gpu: TestGpu,
        ) -> Result<(), String> {
            self.runs.lock().push(args.to_vec());
            // A sample encode writes a file; a decode names its hwaccel; a
            // filter test, its filter.
            // `overlay_cuda`'s test scales on CUDA too: named first.
            let filter = [
                "tonemap_vaapi",
                "overlay_vaapi",
                "overlay_cuda",
                "scale_cuda",
                "libplacebo",
            ]
            .into_iter()
            .find(|f| args.iter().any(|a| a.contains(f)));
            let name = filter
                .map(str::to_owned)
                .or_else(|| {
                    args.iter()
                        .position(|a| a == "-c:v")
                        .map(|i| args[i + 1].clone())
                })
                .or_else(|| {
                    args.iter()
                        .position(|a| a == "-hwaccel")
                        .map(|i| format!("decode_{}", args[i + 1]))
                })
                .unwrap();
            // A 10-bit test encode (PGPU-06) is its encoder's `@10`.
            let ten_bit = args
                .windows(2)
                .any(|w| w[0] == "-pix_fmt" && w[1] == "p010le")
                || args.iter().any(|a| a == "format=p010,hwupload");
            let name = if ten_bit && filter.is_none() {
                format!("{name}@10")
            } else {
                name
            };
            if self.works.iter().any(|w| *w == name) {
                Ok(())
            } else {
                Err("refused".into())
            }
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
        // The dev machine: VA-API works for H.264 and HEVC; NVENC is
        // listed but has no driver. Software is H.264 alone (PGPU-06):
        // libx265 and SVT-AV1 would work, and are not tried.
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
        assert_eq!(names(&e), ["h264_vaapi", "libx264", "hevc_vaapi"]);
        assert_eq!(
            e.video_encoder(VideoCodec::Hevc).unwrap().name,
            "hevc_vaapi"
        );
        assert_eq!(e.video_encoder(VideoCodec::Av1), None);
        // Encode tests, not the samples the decode tests are made with.
        let tried: Vec<String> = ffmpeg
            .runs
            .lock()
            .iter()
            .filter(|args| args.ends_with(&["null".to_owned(), "-".to_owned()]))
            .filter_map(|args| {
                args.iter()
                    .position(|a| a == "-c:v")
                    .map(|i| args[i + 1].clone())
            })
            .collect();
        assert!(
            !tried
                .iter()
                .any(|name| name == "libx265" || name == "libsvtav1"),
            "{tried:?}"
        );
        assert_eq!(e.unusable, [Hardware::Nvenc, Hardware::Qsv]);
        assert!(
            e.hardware_decoders
                .iter()
                .all(|d| d.hardware == Hardware::Vaapi)
        );
        assert_eq!(e.hardware_decoders.len(), 3);
        // Mesa's VA-API tone maps nothing and overlays nothing.
        assert!(!e.gpu_tonemap && !e.gpu_overlay);
    }

    /// PGPU-06: each GPU HEVC and AV1 encoder is tried with a 10-bit test
    /// frame, H.264 not; one that refuses it is still used for 8 bits,
    /// and says what it lacks.
    #[tokio::test]
    async fn gpu_hevc_and_av1_encoders_are_tried_at_10_bits() {
        let ffmpeg = Recorded::new(&[
            "h264_vaapi",
            "hevc_vaapi",
            "hevc_vaapi@10",
            "av1_vaapi",
            "libx264",
        ]);
        let settings = TranscodeSettings {
            hardware: HardwarePolicy::Ordered(vec![Hardware::Vaapi]),
            ..TranscodeSettings::default()
        };
        let e = detect(&ffmpeg, &settings, scratch("ten-bit").await.as_ref()).await;
        let ten: Vec<&str> = e.ten_bit.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(ten, ["hevc_vaapi"]);
        assert!(e.encodes_ten_bit(e.video_encoder(VideoCodec::Hevc).unwrap()));
        assert!(!e.encodes_ten_bit(e.video_encoder(VideoCodec::Av1).unwrap()));
        assert!(e.lacks(Hardware::Vaapi).contains(&"av1 10-bit encode"));
        let ten_bit_runs = ffmpeg
            .runs
            .lock()
            .iter()
            .filter(|args| args.iter().any(|a| a == "format=p010,hwupload"))
            .count();
        assert_eq!(ten_bit_runs, 2, "HEVC and AV1, not H.264");
    }

    /// Each GPU filter is tried with a tiny run, as the encoders are: an
    /// Intel GPU (iHD) runs both, an AMD one neither.
    #[tokio::test]
    async fn gpu_filters_are_found_where_the_driver_runs_them() {
        let ffmpeg = Recorded::new(&["h264_vaapi", "libx264", "decode_vaapi", "tonemap_vaapi"]);
        let e = detect(
            &ffmpeg,
            &TranscodeSettings::default(),
            scratch("gpu-filters").await.as_ref(),
        )
        .await;
        assert!(e.gpu_tonemap);
        assert!(!e.gpu_overlay);
        let joined = ffmpeg
            .runs
            .lock()
            .iter()
            .find(|r| r.iter().any(|a| a.contains("tonemap_vaapi")))
            .expect("a tone map test")
            .join(" ");
        // On the render node detection uses, from HDR10 to SDR.
        assert!(joined.contains("vaapi=va:/dev/dri/renderD128"), "{joined}");
        assert!(joined.contains("smpte2084"), "{joined}");
        let overlay = Recorded::new(&["h264_vaapi", "libx264", "overlay_vaapi"]);
        let e = detect(
            &overlay,
            &TranscodeSettings::default(),
            scratch("gpu-overlay").await.as_ref(),
        )
        .await;
        assert!(!e.gpu_tonemap);
        assert!(e.gpu_overlay);
    }

    /// GPU filters feed a VA-API encoder: without one they are not tried.
    #[tokio::test]
    async fn gpu_filters_are_not_tried_without_a_vaapi_encoder() {
        let ffmpeg = Recorded::new(&["libx264", "tonemap_vaapi", "overlay_vaapi"]);
        let e = detect(
            &ffmpeg,
            &TranscodeSettings::default(),
            scratch("gpu-filters-none").await.as_ref(),
        )
        .await;
        assert!(!e.gpu_tonemap && !e.gpu_overlay);
        let runs = ffmpeg.runs.lock();
        assert!(
            runs.iter()
                .all(|r| !r.iter().any(|a| a.contains("_vaapi=")))
        );
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
        let runs = ffmpeg.runs.lock();
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
        let runs = ffmpeg.runs.lock();
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

    /// A host's ffmpeg by the encoders whose test encode works, and the
    /// samples each hwaccel decodes, as `cuda:hevc-10` (sample
    /// `sample-hevc-10.mkv` through `-hwaccel cuda`).
    struct Host {
        encodes: Vec<&'static str>,
        decodes: Vec<&'static str>,
        /// The CUDA filters whose test works: `scale_cuda`, `overlay_cuda`.
        filters: Vec<&'static str>,
        runs: Mutex<Vec<Vec<String>>>,
        /// Each run's arguments, with the GPU its jail was given.
        gpus: Mutex<Vec<(Vec<String>, TestGpu)>>,
    }

    impl Host {
        fn new(encodes: &[&'static str], decodes: &[&'static str]) -> Self {
            Self {
                encodes: encodes.to_vec(),
                decodes: decodes.to_vec(),
                filters: Vec::new(),
                runs: Mutex::default(),
                gpus: Mutex::default(),
            }
        }

        /// This host, its CUDA filter tests working for `filters`.
        fn running(self, filters: &[&'static str]) -> Self {
            Self {
                filters: filters.to_vec(),
                ..self
            }
        }

        /// The runs whose arguments contain `arg`.
        fn runs_with(&self, arg: &str) -> Vec<Vec<String>> {
            let runs = self.runs.lock();
            runs.iter()
                .filter(|r| r.iter().any(|a| a == arg))
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl Ffmpeg for Host {
        async fn encoders(&self) -> Result<String, String> {
            Recorded::new(&[]).encoders().await
        }

        async fn succeeds(
            &self,
            args: &[String],
            _scratch: &Path,
            gpu: TestGpu,
        ) -> Result<(), String> {
            self.runs.lock().push(args.to_vec());
            self.gpus.lock().push((args.to_vec(), gpu));
            let works = self.works(args);
            if works { Ok(()) } else { Err("refused".into()) }
        }
    }

    impl Host {
        fn works(&self, args: &[String]) -> bool {
            let after = |flag: &str| {
                args.iter()
                    .position(|a| a == flag)
                    .map(|i| args[i + 1].as_str())
            };
            // libplacebo's tests (PGPU-04), by how they get their device.
            if args.iter().any(|a| a.contains("libplacebo=")) {
                let test = if args.iter().any(|a| a == NVIDIA_VULKAN) {
                    "libplacebo nvidia"
                } else if args.iter().any(|a| a == "vulkan=vk@va") {
                    "libplacebo shared"
                } else {
                    "libplacebo own"
                };
                return self.filters.contains(&test);
            }
            // A CUDA filter test decodes the sample too: told apart by its
            // filter, `overlay_cuda`'s first as it scales on CUDA too.
            if let Some(filter) = ["overlay_cuda", "scale_cuda"]
                .into_iter()
                .find(|f| args.iter().any(|a| a.contains(f)))
            {
                return self.filters.contains(&filter);
            }
            if let Some(hwaccel) = after("-hwaccel") {
                let input = Path::new(after("-i").unwrap());
                let stem = input.file_stem().unwrap().to_str().unwrap();
                let sample = stem.strip_prefix("sample-").unwrap();
                return self
                    .decodes
                    .contains(&format!("{hwaccel}:{sample}").as_str());
            }
            if args.iter().any(|a| a.contains("_vaapi=")) {
                return false;
            }
            after("-c:v").is_some_and(|name| self.encodes.contains(&name))
        }
    }

    /// A GTX 1060 (Pascal): NVENC encodes H.264 and HEVC; NVDEC decodes
    /// H.264, 8- and 10-bit HEVC and VP9, but no AV1; CUDA scales and
    /// overlays; libplacebo tone maps on its Vulkan device.
    fn gtx_1060() -> Host {
        gtx_1060_without_filters().running(&["scale_cuda", "overlay_cuda", "libplacebo nvidia"])
    }

    /// `gtx_1060()` with neither CUDA filter working: an ffmpeg without
    /// them, or a driver without `libnvidia-ptxjitcompiler`.
    fn gtx_1060_without_filters() -> Host {
        Host::new(
            &[
                "h264_nvenc",
                "hevc_nvenc",
                "libx264",
                "libx265",
                "libsvtav1",
                "libvpx-vp9",
            ],
            &["cuda:h264-8", "cuda:hevc-8", "cuda:hevc-10", "cuda:vp9-8"],
        )
    }

    fn nvenc_only() -> TranscodeSettings {
        TranscodeSettings {
            hardware: HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
            ..TranscodeSettings::default()
        }
    }

    #[tokio::test]
    async fn nvdec_is_probed_per_codec_and_depth_through_cuda() {
        let host = gtx_1060();
        let e = detect(&host, &nvenc_only(), scratch("nvdec").await.as_ref()).await;
        let found: Vec<String> = e.hardware_decoders.iter().map(|d| d.label()).collect();
        assert_eq!(
            found,
            [
                "h264 (nvdec)",
                "hevc (nvdec)",
                "hevc 10-bit (nvdec)",
                "vp9 (nvdec)"
            ]
        );
        assert!(e.decodes_depth(VideoCodec::Hevc, 10, Hardware::Nvenc));
        assert!(e.decodes_depth(VideoCodec::Hevc, 8, Hardware::Nvenc));
        // Pascal has no AV1, of either depth; and nothing is 12-bit.
        assert!(!e.decodes(VideoCodec::Av1, Hardware::Nvenc));
        assert!(!e.decodes_depth(VideoCodec::Hevc, 12, Hardware::Nvenc));
        // Each decode through CUDA, the frames kept on the GPU so a
        // decode NVDEC cannot do fails rather than falling back.
        // (The CUDA filter tests decode too, and encode: not these.)
        let decodes: Vec<Vec<String>> = host
            .runs_with("-hwaccel")
            .into_iter()
            .filter(|r| !r.iter().any(|a| a == "-c:v"))
            .collect();
        assert_eq!(decodes.len(), 6, "every sample: {decodes:?}");
        for run in &decodes {
            let joined = run.join(" ");
            assert!(
                joined.contains("-hwaccel cuda -hwaccel_output_format cuda"),
                "{joined}"
            );
        }
        // The 10-bit samples are 10-bit.
        let ten_bit = host.runs_with("yuv420p10le");
        let encoders: Vec<&str> = ten_bit
            .iter()
            .map(|r| r[r.iter().position(|a| a == "-c:v").unwrap() + 1].as_str())
            .collect();
        assert_eq!(encoders, ["libx265", "libsvtav1"]);
    }

    /// PGPU-02: what a family's failed tests say it lacks, for the one
    /// line the server logs of a family it names: a GTX 1060's NVENC
    /// has no AV1 encode, and NVDEC no AV1 of either depth.
    #[tokio::test]
    async fn a_family_s_failed_tests_say_what_it_lacks() {
        let e = detect(&gtx_1060(), &nvenc_only(), scratch("lacks").await.as_ref()).await;
        assert_eq!(
            e.lacks(Hardware::Nvenc),
            ["av1 encode", "av1 decode", "av1 10-bit decode"]
        );
        assert!(e.unusable.is_empty());
        let test: Vec<&str> = e.failed.iter().map(|f| f.test.as_str()).collect();
        assert_eq!(
            test,
            [
                "av1_nvenc",
                "decode av1 (nvdec)",
                "decode av1 10-bit (nvdec)"
            ]
        );
        assert!(e.failed.iter().all(|f| f.stderr == "refused"));
    }

    /// The transcoding line at start names what NVDEC decodes.
    #[tokio::test]
    async fn the_summary_lists_nvdec_codecs() {
        let e = detect(
            &gtx_1060(),
            &nvenc_only(),
            scratch("nvdec-summary").await.as_ref(),
        )
        .await;
        assert!(
            e.summary().contains(
                "hardware decoders: h264 (nvdec), hevc (nvdec), hevc 10-bit (nvdec), vp9 (nvdec)"
            ),
            "{}",
            e.summary()
        );
    }

    /// NVDEC is used only for an NVENC encode: with no NVENC encoder its
    /// own samples (10-bit, VP9) are not made, and VA-API is tried on the
    /// 8-bit ones as before.
    #[tokio::test]
    async fn nvdec_samples_are_made_only_where_nvenc_encodes() {
        let host = Host::new(
            &[
                "h264_vaapi",
                "libx264",
                "libx265",
                "libsvtav1",
                "libvpx-vp9",
            ],
            &["vaapi:h264-8", "vaapi:hevc-8", "cuda:hevc-10"],
        );
        let e = detect(
            &host,
            &TranscodeSettings::default(),
            scratch("no-nvenc").await.as_ref(),
        )
        .await;
        assert!(host.runs_with("yuv420p10le").is_empty());
        assert!(host.runs_with("libvpx-vp9").is_empty());
        let found: Vec<String> = e.hardware_decoders.iter().map(|d| d.label()).collect();
        assert_eq!(found, ["h264 (vaapi)", "hevc (vaapi)"]);
    }

    /// Each probe's jail gets the GPU it tests and no other: NVENC and
    /// NVDEC NVIDIA's nodes, VA-API the render node, the software sample
    /// encodes none.
    #[tokio::test]
    async fn each_probe_runs_with_the_gpu_it_tests_only() {
        let host = Host::new(
            &["h264_nvenc", "h264_vaapi", "libx264", "libx265"],
            &["cuda:h264-8", "vaapi:h264-8"],
        );
        let settings = TranscodeSettings {
            hardware: HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi]),
            ..TranscodeSettings::default()
        };
        detect(&host, &settings, scratch("probe-gpus").await.as_ref()).await;
        let gpus = host.gpus.lock();
        let gpu_of = |wanted: &dyn Fn(&str) -> bool| -> Vec<GpuUse> {
            gpus.iter()
                .filter(|(args, _)| args.iter().any(|a| wanted(a)))
                .map(|(_, gpu)| gpu.gpu)
                .collect()
        };
        let nvidia = gpu_of(&|a| a.ends_with("_nvenc") || a == "cuda");
        assert!(!nvidia.is_empty());
        assert!(nvidia.iter().all(|g| *g == GpuUse::Nvidia), "{nvidia:?}");
        let vaapi = gpu_of(&|a| a.ends_with("_vaapi") || a == "vaapi");
        assert!(!vaapi.is_empty());
        assert!(vaapi.iter().all(|g| *g == GpuUse::Vaapi), "{vaapi:?}");
        let software = gpu_of(&|a| a == "libx264" || a == "libx265");
        assert!(!software.is_empty());
        assert!(software.iter().all(|g| *g == GpuUse::None), "{software:?}");
    }

    /// A failed test says what ffmpeg said last on stderr, and only that:
    /// a stand-in ffmpeg that fails as NVENC without its driver does.
    #[tokio::test]
    async fn a_failed_test_says_what_ffmpeg_said_last() {
        let dir = scratch("last-words").await;
        let script = dir.join("ffmpeg");
        let source = dir.join("ffmpeg.txt");
        std::fs::write(
            &source,
            "#!/bin/sh\n\
             echo 'Input #0, lavfi' >&2\n\
             echo '[h264_nvenc @ 0x1] Cannot load libcuda.so.1' >&2\n\
             echo '[vost#0:0/h264_nvenc @ 0x2] Error while opening encoder' >&2\n\
             exit 1\n",
        )
        .unwrap();
        let installed = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&source)
            .arg(&script)
            .status()
            .unwrap();
        assert!(installed.success());
        let ffmpeg = FfmpegCommand::new(Tool::unconfined(&script));
        let why = ffmpeg
            .succeeds(&[], dir.as_ref(), GpuUse::Nvidia.into())
            .await
            .unwrap_err();
        assert_eq!(
            why,
            "exit status: 1: [h264_nvenc @ 0x1] Cannot load libcuda.so.1 / \
             [vost#0:0/h264_nvenc @ 0x2] Error while opening encoder"
        );
    }

    /// PGPU-03: where NVENC and NVDEC work, `scale_cuda` and `overlay_cuda`
    /// are each tried with a short run on NVIDIA's GPU: the H.264 sample
    /// decoded on CUDA, the frames kept there, filtered and encoded on
    /// NVENC.
    #[tokio::test]
    async fn cuda_filters_are_found_where_nvidia_runs_them() {
        let host = gtx_1060();
        let e = detect(&host, &nvenc_only(), scratch("cuda-filters").await.as_ref()).await;
        assert!(e.cuda_scale && e.cuda_overlay);
        // VA-API's are not NVIDIA's.
        assert!(!e.gpu_tonemap && !e.gpu_overlay);
        let gpus = host.gpus.lock();
        let test = |filter: &str| {
            gpus.iter()
                .find(|(args, _)| args.iter().any(|a| a.contains(filter)))
                .cloned()
                .unwrap_or_else(|| panic!("no {filter} test"))
        };
        let (scale, gpu) = test("scale_cuda=w=-2");
        assert_eq!(gpu, GpuUse::Nvidia.into());
        let joined = scale.join(" ");
        assert!(
            joined.contains("-hwaccel cuda -hwaccel_output_format cuda -i ")
                && joined.contains("sample-h264-8.mkv")
                && joined.contains("-vf scale_cuda=w=-2:h=360:format=nv12 -c:v h264_nvenc"),
            "{joined}"
        );
        let (overlay, gpu) = test("overlay_cuda");
        assert_eq!(gpu, GpuUse::Nvidia.into());
        let joined = overlay.join(" ");
        // One named device for the decode and the pictures' upload.
        assert!(
            joined.contains(
                "-init_hw_device cuda=gpu -filter_hw_device gpu -hwaccel cuda \
                 -hwaccel_device gpu -hwaccel_output_format cuda"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("[1:v]hwupload[pictures]")
                && joined.contains("scale_cuda=format=yuv420p")
                && joined.contains("-c:v h264_nvenc"),
            "{joined}"
        );
    }

    /// A CUDA filter test that fails leaves that filter on the CPU, and
    /// the family's one info line says it lacks it.
    #[tokio::test]
    async fn a_cuda_filter_that_fails_is_lacked() {
        let host = gtx_1060_without_filters().running(&["scale_cuda", "libplacebo nvidia"]);
        let e = detect(&host, &nvenc_only(), scratch("cuda-overlay").await.as_ref()).await;
        assert!(e.cuda_scale);
        assert!(!e.cuda_overlay);
        assert_eq!(
            e.lacks(Hardware::Nvenc),
            ["av1 encode", "av1 decode", "av1 10-bit decode", "overlay"]
        );
        let host = gtx_1060_without_filters();
        let e = detect(&host, &nvenc_only(), scratch("cuda-none").await.as_ref()).await;
        assert!(!e.cuda_scale && !e.cuda_overlay);
        let failed: Vec<&str> = e.failed.iter().map(|f| f.test.as_str()).collect();
        assert!(
            failed.ends_with(&["scale_cuda", "overlay_cuda", "libplacebo (shared)"]),
            "{failed:?}"
        );
    }

    /// No CUDA filter test where NVDEC did not decode the H.264 sample,
    /// or no NVENC encoder works: the filters feed one from the other.
    #[tokio::test]
    async fn cuda_filters_are_not_tried_without_nvdec_and_nvenc() {
        let no_nvdec = Host::new(&["h264_nvenc", "libx264", "libx265"], &["cuda:hevc-8"])
            .running(&["scale_cuda", "overlay_cuda"]);
        let e = detect(
            &no_nvdec,
            &nvenc_only(),
            scratch("cuda-no-nvdec").await.as_ref(),
        )
        .await;
        assert!(!e.cuda_scale && !e.cuda_overlay);
        assert!(no_nvdec.runs_with("-filter_complex").is_empty());
        assert!(
            e.lacks(Hardware::Nvenc)
                .iter()
                .all(|l| !l.contains("scale"))
        );
        let vaapi = Host::new(&["h264_vaapi", "libx264"], &["vaapi:h264-8", "cuda:h264-8"])
            .running(&["scale_cuda", "overlay_cuda"]);
        let e = detect(
            &vaapi,
            &TranscodeSettings::default(),
            scratch("cuda-no-nvenc").await.as_ref(),
        )
        .await;
        assert!(!e.cuda_scale && !e.cuda_overlay);
        let runs = vaapi.runs.lock();
        assert!(runs.iter().all(|r| !r.iter().any(|a| a.contains("_cuda"))));
    }

    /// The transcoding line says what each family filters on the GPU.
    #[test]
    fn the_summary_names_each_family_s_gpu_filters() {
        let mut e = Encoders::software_baseline();
        assert!(
            e.summary().contains("GPU filters: none;"),
            "{}",
            e.summary()
        );
        (e.gpu_tonemap, e.gpu_overlay) = (true, true);
        (e.cuda_scale, e.cuda_overlay) = (true, true);
        assert!(
            e.summary()
                .contains("GPU filters: vaapi: tone map, overlay; nvenc: scale, overlay;"),
            "{}",
            e.summary()
        );
        e.gpu_tonemap = false;
        e.gpu_overlay = false;
        e.cuda_overlay = false;
        assert!(
            e.summary().contains("GPU filters: nvenc: scale;"),
            "{}",
            e.summary()
        );
    }

    /// A host with an AMD GPU (VA-API encodes H.264) and a GTX 1060.
    fn amd_and_nvidia(libplacebo: &[&'static str]) -> Host {
        Host::new(
            &["h264_vaapi", "h264_nvenc", "libx264", "libx265"],
            &["vaapi:h264-8", "cuda:h264-8"],
        )
        .running(libplacebo)
    }

    fn both() -> TranscodeSettings {
        TranscodeSettings {
            hardware: HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi]),
            ..TranscodeSettings::default()
        }
    }

    /// The runs with a libplacebo filter, in order, with their jail's GPU.
    fn libplacebo_runs(host: &Host) -> Vec<(String, TestGpu)> {
        host.gpus
            .lock()
            .iter()
            .filter(|(args, _)| args.iter().any(|a| a.contains("libplacebo=")))
            .map(|(args, gpu)| (args.join(" "), gpu.clone()))
            .collect()
    }

    /// PGPU-04: libplacebo's tone map is tried on each family whose H.264
    /// encoder works: on its own Vulkan device on the VA-API GPU, frames
    /// from memory uploaded to `h264_vaapi`; on ffmpeg's NVIDIA Vulkan
    /// device, downloaded to NVENC. Each run is the family's GPU, through
    /// Vulkan.
    #[tokio::test]
    async fn libplacebo_is_found_per_family_with_its_device() {
        let host = amd_and_nvidia(&["libplacebo own", "libplacebo nvidia"]);
        let e = detect(&host, &both(), scratch("libplacebo").await.as_ref()).await;
        assert_eq!(
            e.libplacebo,
            [
                Libplacebo {
                    hardware: Hardware::Vaapi,
                    device: VulkanDevice::Own
                },
                Libplacebo {
                    hardware: Hardware::Nvenc,
                    device: VulkanDevice::Shared
                },
            ]
        );
        let runs = libplacebo_runs(&host);
        assert_eq!(runs.len(), 2, "{runs:?}");
        let (own, gpu) = &runs[0];
        assert_eq!(gpu.gpu, GpuUse::Vaapi);
        assert!(gpu.vulkan.is_some());
        assert!(
            own.contains("-init_hw_device vaapi=va:/dev/dri/renderD128 -filter_hw_device va ")
                && own.contains("smpte2084")
                && own.contains(
                    "-vf libplacebo=w=-2:h=360:tonemapping=bt.2390:colorspace=bt709:\
                     color_primaries=bt709:color_trc=bt709:range=tv:format=nv12,hwupload \
                     -c:v h264_vaapi"
                )
                && !own.contains("vulkan="),
            "{own}"
        );
        let (nvidia, gpu) = &runs[1];
        assert_eq!(gpu.gpu, GpuUse::Nvidia);
        assert_eq!(gpu.vulkan, Some(Vulkan::default()));
        assert!(
            nvidia.contains("-init_hw_device vulkan=vk:NVIDIA -filter_hw_device vk ")
                && nvidia.contains("-vf hwupload,libplacebo=w=-2:h=360:")
                && nvidia.contains("format=nv12,hwdownload,format=nv12 -c:v h264_nvenc"),
            "{nvidia}"
        );
        assert!(!e.lacks(Hardware::Vaapi).contains(&"libplacebo tone map"));
    }

    /// With `experimental.libplacebo_shared_on_amd`, the VA-API GPU tries
    /// ffmpeg's Vulkan device first, derived from the VA-API one, and
    /// falls back to libplacebo's own where that fails (RADV today),
    /// lacking nothing; without it, shared is never tried.
    #[tokio::test]
    async fn shared_on_amd_is_tried_first_with_the_switch() {
        let on = TranscodeSettings {
            libplacebo_shared_on_amd: true,
            ..TranscodeSettings::default()
        };
        let radv = amd_and_nvidia(&["libplacebo own"]);
        let e = detect(&radv, &on, scratch("shared-amd-fails").await.as_ref()).await;
        assert_eq!(e.libplacebo(Hardware::Vaapi), Some(VulkanDevice::Own));
        assert!(!e.lacks(Hardware::Vaapi).contains(&"libplacebo tone map"));
        let runs: Vec<String> = libplacebo_runs(&radv)
            .into_iter()
            .map(|(args, _)| args)
            .filter(|args| args.contains("h264_vaapi"))
            .collect();
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert!(
            runs[0].contains("-init_hw_device vulkan=vk@va -filter_hw_device vk")
                && runs[0].contains(
                    "hwupload=derive_device=vaapi,hwmap=derive_device=vulkan,libplacebo="
                )
                && runs[0].contains("hwdownload,format=nv12,hwupload=derive_device=vaapi"),
            "{}",
            runs[0]
        );
        assert!(!runs[1].contains("vulkan="), "{}", runs[1]);

        let fixed = amd_and_nvidia(&["libplacebo shared", "libplacebo own"]);
        let e = detect(&fixed, &on, scratch("shared-amd-works").await.as_ref()).await;
        assert_eq!(e.libplacebo(Hardware::Vaapi), Some(VulkanDevice::Shared));
        let vaapi_runs = libplacebo_runs(&fixed)
            .into_iter()
            .filter(|(args, _)| args.contains("h264_vaapi"))
            .count();
        assert_eq!(vaapi_runs, 1);

        let off = amd_and_nvidia(&["libplacebo shared", "libplacebo own"]);
        let e = detect(
            &off,
            &TranscodeSettings::default(),
            scratch("shared-amd-off").await.as_ref(),
        )
        .await;
        assert_eq!(e.libplacebo(Hardware::Vaapi), Some(VulkanDevice::Own));
        assert!(
            libplacebo_runs(&off)
                .iter()
                .all(|(args, _)| !args.contains("vk@va"))
        );
    }

    /// A family where libplacebo fails lacks it, in the start's info
    /// line; a family with no H.264 encoder is not tried.
    #[tokio::test]
    async fn a_failed_libplacebo_test_is_lacked() {
        let host = amd_and_nvidia(&[]);
        let e = detect(&host, &both(), scratch("libplacebo-fails").await.as_ref()).await;
        assert!(e.libplacebo.is_empty());
        assert!(e.lacks(Hardware::Vaapi).contains(&"libplacebo tone map"));
        assert!(e.lacks(Hardware::Nvenc).contains(&"libplacebo tone map"));
        let tests: Vec<&str> = e.failed.iter().map(|f| f.test.as_str()).collect();
        assert!(tests.contains(&"libplacebo (own)") && tests.contains(&"libplacebo (shared)"));

        let software =
            Host::new(&["libx264"], &[]).running(&["libplacebo own", "libplacebo nvidia"]);
        let e = detect(
            &software,
            &TranscodeSettings::default(),
            scratch("libplacebo-none").await.as_ref(),
        )
        .await;
        assert!(e.libplacebo.is_empty());
        assert!(libplacebo_runs(&software).is_empty());
    }

    /// The transcoding line says per family which tone maps run on the
    /// GPU, in auto's order.
    #[test]
    fn the_summary_names_each_family_s_gpu_tone_maps() {
        let mut e = Encoders::software_baseline();
        assert!(
            e.summary().ends_with("; GPU tone maps: none"),
            "{}",
            e.summary()
        );
        e.gpu_tonemap = true;
        e.libplacebo = vec![
            Libplacebo {
                hardware: Hardware::Vaapi,
                device: VulkanDevice::Own,
            },
            Libplacebo {
                hardware: Hardware::Nvenc,
                device: VulkanDevice::Shared,
            },
        ];
        assert!(
            e.summary().ends_with(
                "; GPU tone maps: vaapi: VA-API, libplacebo (own); nvenc: libplacebo (shared)"
            ),
            "{}",
            e.summary()
        );
    }

    /// A forced method no GPU runs is warned of; auto and the CPU never.
    #[test]
    fn a_forced_tone_map_no_gpu_runs_is_told_apart() {
        let mut e = Encoders::software_baseline();
        assert!(!e.forced_nowhere(ToneMapSetting::Auto));
        assert!(!e.forced_nowhere(ToneMapSetting::Cpu));
        assert!(e.forced_nowhere(ToneMapSetting::Gpu));
        assert!(e.forced_nowhere(ToneMapSetting::Libplacebo));
        e.libplacebo = vec![Libplacebo {
            hardware: Hardware::Nvenc,
            device: VulkanDevice::Shared,
        }];
        assert!(!e.forced_nowhere(ToneMapSetting::Libplacebo));
        assert!(e.forced_nowhere(ToneMapSetting::Gpu));
        e.gpu_tonemap = true;
        assert!(!e.forced_nowhere(ToneMapSetting::Gpu));
    }

    #[test]
    fn a_tone_map_setting_is_one_of_four_words() {
        for word in ["auto", "cpu", "gpu", "libplacebo"] {
            assert_eq!(word.parse::<ToneMapSetting>().unwrap().as_str(), word);
        }
        let err = "vulkan".parse::<ToneMapSetting>().unwrap_err();
        assert!(err.contains("\"libplacebo\""), "{err}");
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
