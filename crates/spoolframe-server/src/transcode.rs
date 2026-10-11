//! The encoders this server can use (P2-02), on each GPU it has
//! (PGPU-05): detected at start and again on each config reload (SIGHUP,
//! PX-33: `runtime::reload`), on the cards listed at start; read by the
//! planner and the jobs.

use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spoolframe_media::encoders::{
    Encoders, Ffmpeg, FfmpegCommand, Libplacebo, TranscodeSettings, VideoEncoder,
};
use spoolframe_media::gpus::{self, Card, Found, GpuEntry, HostCard, Missing};
use spoolframe_media::sandbox::Tool;

use crate::encode::job::CardLimit;

/// One GPU the server uses: a card an entry of `transcode.gpus` took,
/// what detection found it does, and ffmpeg confined to its nodes.
#[derive(Debug)]
pub struct Gpu {
    pub found: Found,
    /// ffmpeg, its runs given this card's nodes and no other's.
    pub ffmpeg: Tool,
    /// GPU video encodes at once here: its entry's `max_jobs`; `None`
    /// for `transcode.max_gpu_jobs`.
    pub max_jobs: Option<usize>,
}

impl Gpu {
    /// What detection found here: this card's API's encoders and
    /// decoders, and software's.
    pub fn encoders(&self) -> &Encoders {
        &self.found.encoders
    }

    /// The render node its VA-API runs name.
    pub fn vaapi_device(&self) -> PathBuf {
        self.found.card.vaapi_device()
    }

    pub fn card(&self) -> &Card {
        &self.found.card
    }

    /// Its place in the encode budget: its own count of GPU jobs.
    pub fn limit(&self) -> CardLimit {
        CardLimit {
            key: self.found.card.key(),
            max_jobs: self.max_jobs,
        }
    }
}

/// What detection found: every GPU, and everything they and the CPU
/// encode together.
#[derive(Debug, Clone)]
pub struct Detected {
    /// What this server encodes, on any GPU or the CPU: what the planner
    /// plans with and the status lists.
    pub encoders: Arc<Encoders>,
    /// The GPUs, in the order jobs try them (`gpus::rank`).
    pub gpus: Vec<Arc<Gpu>>,
    /// What the CPU encodes alone: a conversion on no GPU runs with it.
    pub software: Arc<Encoders>,
    /// The host's GPUs that do none of the work, and why: for the
    /// Server page, as the start lines say them.
    pub others: Vec<Other>,
    /// The entries of `transcode.gpus` that took no card, as the log
    /// says them.
    pub notes: Vec<String>,
}

/// A GPU of the host's that encodes nothing here.
#[derive(Debug, Clone)]
pub struct Other {
    pub host: HostCard,
    /// The driver's name for it where detection read one, else
    /// [`HostCard::fallback_name`].
    pub name: String,
    pub why: OtherWhy,
}

/// Why a GPU does none of the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtherWhy {
    /// No entry of `transcode.gpus` takes it.
    NoEntry,
    /// `entry` takes it, for `api`, and none of its tests passed: the
    /// first that failed is `test`.
    NothingWorks {
        api: spoolframe_media::encoders::Hardware,
        entry: String,
        test: Option<String>,
    },
}

impl Detected {
    /// Before detection: software only, no GPU.
    pub fn software_baseline() -> Self {
        let software = Arc::new(Encoders::software_baseline());
        Self {
            encoders: software.clone(),
            gpus: Vec::new(),
            software,
            others: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// `gpus` as detection would have found them, software's baseline
    /// beside: for tests.
    #[cfg(test)]
    pub(crate) fn with_gpus(gpus: Vec<Gpu>) -> Self {
        let software = Encoders::software_baseline();
        let gpus: Vec<Arc<Gpu>> = gpus.into_iter().map(Arc::new).collect();
        Self {
            encoders: Arc::new(merge(&software, &gpus)),
            gpus,
            software: Arc::new(software),
            others: Vec::new(),
            notes: Vec::new(),
        }
    }
}

/// A GPU for tests: card `n` of `api` (PCI `0000:0n:00.0`, render node
/// `renderD(128 + n)`, NVIDIA index `n`), its detection `encoders`, run
/// with a plain `ffmpeg`.
#[cfg(test)]
pub(crate) fn test_gpu(
    n: u8,
    api: spoolframe_media::encoders::Hardware,
    kind: spoolframe_media::gpus::Kind,
    encoders: Encoders,
) -> Gpu {
    use spoolframe_media::encoders::Hardware;
    use spoolframe_media::gpus::{Class, NvidiaCard};
    let pci = format!("0000:{n:02x}:00.0");
    let nvidia = api == Hardware::Nvenc;
    let host = HostCard {
        vendor: if nvidia { 0x10de } else { 0x1002 },
        device_id: 0,
        driver: if nvidia { "nvidia" } else { "amdgpu" }.into(),
        render: (!nvidia).then(|| PathBuf::from(format!("/dev/dri/renderD{}", 128 + u32::from(n)))),
        by_path: None,
        nvidia: nvidia.then_some(NvidiaCard {
            index: u32::from(n),
            minor: u32::from(n),
            model: None,
        }),
        class: Class {
            kind,
            how: "test".into(),
        },
        pci,
    };
    let card = Card {
        api,
        host,
        entry: 0,
    };
    Gpu {
        found: Found {
            capability: gpus::capability(&encoders, api),
            name: format!("card {n}"),
            card,
            encoders,
        },
        ffmpeg: Tool::unconfined("ffmpeg"),
        max_jobs: None,
    }
}

pub struct Transcoding {
    /// ffmpeg confined, with no GPU: each card's is made from it.
    ffmpeg: Tool,
    entries: Vec<GpuEntry>,
    /// What detection reads besides the cards: `tone_map` and the
    /// experimental switches (PGPU-04), as the server started.
    settings: TranscodeSettings,
    /// The host's GPUs, listed at start.
    host: Vec<HostCard>,
    /// Where the host's device nodes are: `/dev`.
    dev: PathBuf,
    /// Where detection writes its test samples.
    scratch: PathBuf,
    current: RwLock<Arc<Detected>>,
}

impl std::fmt::Debug for Transcoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcoding")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl Transcoding {
    /// Software only until [`Transcoding::detect`] has run; and with no
    /// card of the host's (tests) until [`Transcoding::on_host`].
    pub fn new(ffmpeg: Tool, entries: Vec<GpuEntry>, scratch: PathBuf) -> Self {
        Self {
            ffmpeg,
            entries,
            settings: TranscodeSettings::default(),
            host: Vec::new(),
            dev: PathBuf::from("/dev"),
            scratch,
            current: RwLock::new(Arc::new(Detected::software_baseline())),
        }
    }

    /// Detection with `settings`' tone map and experimental switches
    /// (PGPU-04); the defaults until this.
    pub fn with_settings(self, settings: TranscodeSettings) -> Self {
        Self { settings, ..self }
    }

    /// The host's GPUs (`gpus::host_cards`), their nodes under `dev`.
    pub fn on_host(self, host: Vec<HostCard>, dev: &Path) -> Self {
        Self {
            host,
            dev: dev.to_path_buf(),
            ..self
        }
    }

    /// `transcode.gpus`, as the server started with it.
    pub fn entries(&self) -> &[GpuEntry] {
        &self.entries
    }

    pub fn encoders(&self) -> Arc<Encoders> {
        self.current.read().encoders.clone()
    }

    /// The GPUs found and what they do.
    pub fn detected(&self) -> Arc<Detected> {
        self.current.read().clone()
    }

    /// Find what works now, on each card the entries take, and use it
    /// from here on.
    pub async fn detect(&self) {
        let (cards, missing) = gpus::select(&self.entries, &self.host);
        let notes = missing
            .iter()
            .map(|missed| log_missing(&self.entries, missed))
            .collect();
        let mut others = Vec::new();
        for host in self
            .host
            .iter()
            .filter(|h| !cards.iter().any(|c| c.host.pci == h.pci))
        {
            others.push(Other {
                host: host.clone(),
                name: host.fallback_name(),
                why: OtherWhy::NoEntry,
            });
            tracing::info!(
                pci = host.pci,
                driver = host.driver,
                "GPU {} ({}, {}): {} ({}); no transcode.gpus entry takes it",
                host.fallback_name(),
                host.pci,
                host.driver,
                host.class.kind.as_str(),
                host.class.how,
            );
        }
        let software = FfmpegCommand::new(self.ffmpeg.clone());
        let ffmpeg_for = |card: &Card| -> Box<dyn Ffmpeg> {
            Box::new(FfmpegCommand::new(self.card_ffmpeg(card)))
        };
        let (baseline, found) =
            gpus::detect_cards(&software, &ffmpeg_for, cards, &self.settings, &self.scratch).await;
        // Every entry is the default one: a card that does nothing is how
        // the host is, as a family under `auto` was.
        let quiet = self.entries == GpuEntry::defaults();
        for f in found
            .iter()
            .filter(|f| f.encoders.unusable.contains(&f.card.api))
        {
            log_no_encoder(&self.entries, f, quiet);
        }
        // A card that encodes nothing may still decode for a software
        // encode, as a family's decoders were kept; one that does nothing
        // at all is left out.
        let (usable, useless): (Vec<Found>, Vec<Found>) =
            found.into_iter().partition(|f| f.capability > 0);
        others.extend(useless.into_iter().map(|f| {
            Other {
                why: OtherWhy::NothingWorks {
                    api: f.card.api,
                    entry: entry_label(&self.entries, f.card.entry),
                    test: f
                        .encoders
                        .failed
                        .iter()
                        .find(|failed| failed.hardware == f.card.api)
                        .map(|failed| failed.test.clone()),
                },
                name: f.name,
                host: f.card.host,
            }
        }));
        let gpus: Vec<Arc<Gpu>> = gpus::rank(usable, &self.entries)
            .into_iter()
            .map(|found| {
                Arc::new(Gpu {
                    ffmpeg: self.card_ffmpeg(&found.card),
                    max_jobs: self.entries.get(found.card.entry).and_then(|e| e.max_jobs),
                    found,
                })
            })
            .collect();
        for gpu in &gpus {
            log_gpu(&self.entries, gpu);
        }
        let encoders = merge(&baseline, &gpus);
        tracing::info!(encoders = %encoders.summary(), gpus = gpus.len(), "transcoding");
        // PGPU-04's warning, asked of every card together (`detect_cards`
        // leaves it out of each card's detection).
        if encoders.forced_nowhere(self.settings.tone_map) {
            tracing::warn!(
                tone_map = self.settings.tone_map.as_str(),
                "transcode.tone_map forces a tone map no GPU here runs; HDR is tone mapped on the CPU"
            );
        }
        *self.current.write() = Arc::new(Detected {
            encoders: Arc::new(encoders),
            gpus,
            software: Arc::new(baseline),
            others,
            notes,
        });
    }

    /// ffmpeg as the server runs it, given `card`'s nodes alone.
    fn card_ffmpeg(&self, card: &Card) -> Tool {
        Tool::new(
            self.ffmpeg.program(),
            self.ffmpeg.sandbox().with_gpu(card.devices(&self.dev)),
        )
    }
}

/// `entry N (vaapi pick=dgpu)`, for the log.
pub(crate) fn entry_label(entries: &[GpuEntry], n: usize) -> String {
    match entries.get(n) {
        Some(entry) => format!("transcode.gpus entry {} ({})", n + 1, entry.describe()),
        None => format!("transcode.gpus entry {}", n + 1),
    }
}

/// An entry that took no card: a pick that found none is how the host
/// is; a device named and missing is a mistake. What it says, for the
/// Server page too.
fn log_missing(entries: &[GpuEntry], missed: &Missing) -> String {
    let entry = entry_label(entries, missed.entry);
    if missed.fixed {
        tracing::warn!(why = missed.why, "{entry} names a GPU that is not here");
        format!("{entry} names a GPU that is not here: {}", missed.why)
    } else {
        tracing::info!(why = missed.why, "{entry} takes no GPU");
        format!("{entry} takes no GPU: {}", missed.why)
    }
}

/// A card none of whose encoders works: no encode goes to it. A warning
/// when the operator asked for its API (entries of their own, as a family
/// named in `transcode.hardware` was), with the first failed test and
/// what ffmpeg said; under the default entries only a line.
fn log_no_encoder(entries: &[GpuEntry], found: &Found, quiet: bool) {
    let card = &found.card;
    let first = found
        .encoders
        .failed
        .iter()
        .find(|f| f.hardware == card.api);
    let entry = entry_label(entries, card.entry);
    if quiet {
        tracing::info!(
            gpu = found.name,
            place = card.place(),
            test = first.map(|f| f.test.as_str()),
            "{entry}: no {} encoder works on {}",
            card.api.as_str(),
            found.name,
        );
        return;
    }
    tracing::warn!(
        gpu = found.name,
        place = card.place(),
        test = first.map(|f| f.test.as_str()),
        stderr = first.map(|f| f.stderr.as_str()),
        "{entry} takes a GPU none of whose {} encoders works here",
        card.api.as_str(),
    );
}

/// One line a card at start: what it is, how it was classified, which
/// entry took it, what it does; and, if it lacks something, one line
/// saying what (PGPU-02's, now a card).
fn log_gpu(entries: &[GpuEntry], gpu: &Gpu) {
    let found = &gpu.found;
    let card = &found.card;
    let api = card.api;
    let encoders = &found.encoders;
    let names = |v: Vec<String>| {
        if v.is_empty() {
            "none".to_owned()
        } else {
            v.join(", ")
        }
    };
    let encodes = names(
        encoders
            .video
            .iter()
            .filter(|e| e.hardware == Some(api))
            .map(|e| e.name.clone())
            .collect(),
    );
    let decodes = names(
        encoders
            .hardware_decoders
            .iter()
            .filter(|d| d.hardware == api)
            .map(|d| d.label())
            .collect(),
    );
    // The summary's own words for the GPU filters: what detection says
    // of them is its business.
    let summary = encoders.summary();
    let filters = summary
        .split_once("GPU filters: ")
        .map_or("none", |(_, filters)| filters);
    let limit = match gpu.max_jobs {
        Some(max) => format!("up to {max} jobs"),
        None => "up to transcode.max_gpu_jobs".to_owned(),
    };
    tracing::info!(
        gpu = found.name,
        api = api.as_str(),
        place = card.place(),
        kind = card.host.class.kind.as_str(),
        classified = card.host.class.how,
        capability = found.capability,
        "GPU {} ({}, {}): {} ({}); {}, {limit}; encodes {encodes}; decodes {decodes}; \
         GPU filters {filters}",
        found.name,
        api.as_str(),
        card.place(),
        card.host.class.kind.as_str(),
        card.host.class.how,
        entry_label(entries, card.entry),
    );
    let lacks = encoders.lacks(api);
    if !lacks.is_empty() {
        tracing::info!(
            gpu = found.name,
            place = card.place(),
            "{} lacks: {}",
            api.as_str(),
            lacks.join(", ")
        );
    }
}

/// Everything the GPUs and the CPU encode, as one detection over all of
/// them would list it: each codec's GPU encoders in the GPUs' order, then
/// its software ones, the codecs in detection's order; every GPU decoder;
/// a GPU filter where any GPU has it. What the planner plans with: a
/// codec is offered if anything here encodes it, and the job then picks
/// the card ([`crate::encode::pick`]).
///
/// Each of [`Encoders`]' fields: `video`, `hardware_decoders` as above;
/// `audio` software's (no GPU encodes audio); the GPU filters
/// (`gpu_tonemap`, `gpu_overlay`, `cuda_scale`, `cuda_overlay`) where any
/// card has it; `libplacebo` (PGPU-04) one per API, the first card's in
/// the jobs' order that tone maps with it, as the status and a forced
/// `tone_map`'s warning read it (a job reads its own card's); `unusable`
/// and `failed` software's, each card's being logged as its own
/// (`log_no_encoder`, `log_gpu`). A field added to [`Encoders`] for a GPU
/// needs a line here, or it reads as software's.
fn merge(software: &Encoders, gpus: &[Arc<Gpu>]) -> Encoders {
    let lists: Vec<&Encoders> = gpus
        .iter()
        .map(|g| g.encoders())
        .chain([software])
        .collect();
    let every = || lists.iter().flat_map(|e| e.video.iter());
    let mut codecs = Vec::new();
    for encoder in every() {
        if !codecs.contains(&encoder.codec) {
            codecs.push(encoder.codec);
        }
    }
    let mut video: Vec<VideoEncoder> = Vec::new();
    for codec in codecs {
        for gpu_first in [true, false] {
            for encoder in every() {
                if encoder.codec == codec
                    && encoder.hardware.is_some() == gpu_first
                    && !video.iter().any(|v| v.name == encoder.name)
                {
                    video.push(encoder.clone());
                }
            }
        }
    }
    let mut hardware_decoders = Vec::new();
    for decoder in lists.iter().flat_map(|e| e.hardware_decoders.iter()) {
        if !hardware_decoders.contains(decoder) {
            hardware_decoders.push(decoder.clone());
        }
    }
    let mut libplacebo: Vec<Libplacebo> = Vec::new();
    for placebo in lists.iter().flat_map(|e| e.libplacebo.iter()) {
        if !libplacebo.iter().any(|l| l.hardware == placebo.hardware) {
            libplacebo.push(*placebo);
        }
    }
    let mut ten_bit: Vec<VideoEncoder> = Vec::new();
    for encoder in lists.iter().flat_map(|e| e.ten_bit.iter()) {
        if !ten_bit.iter().any(|t| t.name == encoder.name) {
            ten_bit.push(encoder.clone());
        }
    }
    let any = |field: fn(&Encoders) -> bool| lists.iter().any(|e| field(e));
    Encoders {
        video,
        audio: software.audio.clone(),
        hardware_decoders,
        gpu_tonemap: any(|e| e.gpu_tonemap),
        gpu_overlay: any(|e| e.gpu_overlay),
        cuda_scale: any(|e| e.cuda_scale),
        cuda_overlay: any(|e| e.cuda_overlay),
        libplacebo,
        ten_bit,
        unusable: software.unusable.clone(),
        failed: software.failed.clone(),
    }
}

/// Detect now, in the background. A config reload (SIGHUP) detects again
/// (`runtime::reload`, PX-33).
pub fn spawn_detection(transcoding: Arc<Transcoding>) {
    tokio::spawn(async move {
        transcoding.detect().await;
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spoolframe_admin_api::{LogLevel, LogQuery};
    use spoolframe_media::encoders::{Hardware, HardwarePolicy};
    use spoolframe_media::gpus::{Class, Kind, NvidiaCard, Pick, Which};
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;
    use crate::logs::{LogRing, RingLayer};

    /// A stand-in ffmpeg that lists NVENC and libx264, encodes with
    /// libx264, and fails with NVENC as it does without its driver.
    const NO_CUDA: &str = "#!/bin/sh\n\
         case \"$*\" in\n\
         *-encoders*) printf ' ------\\n V....D h264_nvenc NVIDIA\\n V....D libx264 x264\\n' ;;\n\
         *h264_nvenc*) echo 'Input #0, lavfi' >&2\n\
         \x20 echo '[h264_nvenc @ 0x1] Cannot load libcuda.so.1' >&2; exit 1 ;;\n\
         esac\n";

    /// A stand-in ffmpeg for a GTX 1060: NVENC encodes H.264, not AV1.
    const PASCAL: &str = "#!/bin/sh\n\
         case \"$*\" in\n\
         *-encoders*) printf ' ------\\n V....D h264_nvenc NVIDIA\\n V....D av1_nvenc NVIDIA\\n V....D libx264 x264\\n' ;;\n\
         *av1_nvenc*) echo '[av1_nvenc @ 0x1] No capable devices found' >&2; exit 1 ;;\n\
         esac\n";

    /// A stand-in ffmpeg for two AMD cards: VA-API encodes H.264 on both
    /// and HEVC on renderD128 only; the verbose run names each card.
    const TWO_AMD: &str = "#!/bin/sh\n\
         case \"$*\" in\n\
         *-encoders*) printf ' ------\\n V....D h264_vaapi VAAPI\\n V....D hevc_vaapi VAAPI\\n V....D libx264 x264\\n' ;;\n\
         *verbose*renderD128*) echo '[VAAPI @ 0x1] VAAPI driver: Mesa Gallium driver 26.1.8 for AMD Radeon RX 7900 XTX (radeonsi).' >&2 ;;\n\
         *verbose*renderD129*) echo '[VAAPI @ 0x1] VAAPI driver: Mesa Gallium driver 26.1.8 for AMD Ryzen 9 7950X3D 16-Core Processor (radeonsi).' >&2 ;;\n\
         *hevc_vaapi*renderD129*|*renderD129*hevc_vaapi*) exit 1 ;;\n\
         *tonemap_vaapi*|*overlay_vaapi*) exit 1 ;;\n\
         *-hwaccel*) exit 1 ;;\n\
         esac\n";

    fn geforce() -> HostCard {
        HostCard {
            pci: "0000:01:00.0".into(),
            vendor: 0x10de,
            device_id: 0x1c03,
            driver: "nvidia".into(),
            render: None,
            by_path: None,
            nvidia: Some(NvidiaCard {
                index: 0,
                minor: 0,
                model: Some("NVIDIA GeForce GTX 1060 6GB".into()),
            }),
            class: gpus::classify(0x10de, "nvidia", "0000:01:00.0", None),
        }
    }

    fn radeon(pci: &str, render: &str, kind: Kind) -> HostCard {
        HostCard {
            pci: pci.into(),
            vendor: 0x1002,
            device_id: 0x744c,
            driver: "amdgpu".into(),
            render: Some(PathBuf::from(render)),
            by_path: None,
            nvidia: None,
            class: Class {
                kind,
                how: "amdgpu fusion flag: test".into(),
            },
        }
    }

    /// The lines at `level` or worse that mention `text`.
    fn lines(ring: &LogRing, level: LogLevel, text: &str) -> Vec<String> {
        ring.page(&LogQuery {
            level: Some(level),
            text: Some(text.into()),
            ..LogQuery::default()
        })
        .lines
        .into_iter()
        .map(|line| line.text)
        .collect()
    }

    /// The lines at `level` or worse that mention libcuda.
    fn about_cuda(ring: &LogRing, level: LogLevel) -> Vec<String> {
        lines(ring, level, "libcuda")
    }

    /// Detection of `entries` on `host` with the stand-in ffmpeg
    /// `script`, what it logged, and what it found.
    async fn detect_on(
        name: &str,
        entries: Vec<GpuEntry>,
        host: Vec<HostCard>,
        script: &str,
    ) -> (Arc<LogRing>, Arc<Detected>) {
        detect_on_with(name, entries, host, script, TranscodeSettings::default()).await
    }

    /// [`detect_on`] with `settings`' tone map and experimental switches.
    async fn detect_on_with(
        name: &str,
        entries: Vec<GpuEntry>,
        host: Vec<HostCard>,
        script: &str,
        settings: TranscodeSettings,
    ) -> (Arc<LogRing>, Arc<Detected>) {
        let dir = spoolframe_test_dir::TestDir::new(name).await;
        let ffmpeg_path = dir.join("ffmpeg");
        crate::test_support::write_script(&ffmpeg_path, script);
        let ring = Arc::new(LogRing::default());
        let _logged = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(RingLayer(ring.clone())),
        );
        let transcoding =
            Transcoding::new(Tool::unconfined(ffmpeg_path), entries, dir.join("scratch"))
                .with_settings(settings)
                .on_host(host, Path::new("/dev"));
        transcoding.detect().await;
        (ring, transcoding.detected())
    }

    async fn detect_with(name: &str, hardware: HardwarePolicy, script: &str) -> Arc<LogRing> {
        let entries = GpuEntry::from_hardware(&hardware, None);
        detect_on(name, entries, vec![geforce()], script).await.0
    }

    /// NVENC named in transcode.hardware, its card there, no encoder of it
    /// working: one warning, with what its first failed test's ffmpeg
    /// said last.
    #[tokio::test]
    async fn a_named_family_s_failed_test_is_a_warning_with_ffmpeg_s_last_words() {
        let ring = detect_with(
            "spoolframe-transcode-named",
            HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
            NO_CUDA,
        )
        .await;
        let warned = about_cuda(&ring, LogLevel::Warn);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("test=\"h264_nvenc\""), "{}", warned[0]);
        assert!(
            warned[0].contains(
                "exit status: 1: Input #0, lavfi / [h264_nvenc @ 0x1] Cannot load libcuda.so.1"
            ),
            "{}",
            warned[0]
        );
    }

    /// Under the default entries the same failure is a line, its stderr
    /// only for debugging: most hosts lack some API.
    #[tokio::test]
    async fn a_default_card_s_failed_test_is_no_warning() {
        let ring = detect_with("spoolframe-transcode-auto", HardwarePolicy::Auto, NO_CUDA).await;
        assert_eq!(lines(&ring, LogLevel::Warn, ""), Vec::<String>::new());
        let debug = about_cuda(&ring, LogLevel::Debug);
        assert_eq!(debug.len(), 1, "{debug:?}");
        assert!(
            debug[0].contains("Cannot load libcuda.so.1"),
            "{}",
            debug[0]
        );
        let said = lines(&ring, LogLevel::Info, "no nvenc encoder works");
        assert_eq!(said.len(), 1, "{said:?}");
    }

    /// PGPU-02: a card that works but lacks a codec is not a warning: one
    /// line says what it lacks, and each failed test's stderr is at debug.
    #[tokio::test]
    async fn a_named_family_lacking_a_codec_says_so_once_not_as_a_warning() {
        let ring = detect_with(
            "spoolframe-transcode-lacks",
            HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
            PASCAL,
        )
        .await;
        assert_eq!(lines(&ring, LogLevel::Warn, ""), Vec::<String>::new());
        let lacks = lines(&ring, LogLevel::Info, "lacks");
        assert_eq!(lacks.len(), 1, "{lacks:?}");
        assert!(lacks[0].contains("nvenc lacks: av1 encode"), "{}", lacks[0]);
        assert!(
            lacks[0].contains("NVIDIA GeForce GTX 1060 6GB"),
            "{}",
            lacks[0]
        );
        let why = lines(&ring, LogLevel::Debug, "No capable devices");
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("test=\"av1_nvenc\""), "{}", why[0]);
    }

    /// PGPU-05: what each card lacks is said under the default entries
    /// too, now that it is per card: one line, no warning.
    #[tokio::test]
    async fn a_default_card_lacking_a_codec_says_so_once() {
        let ring = detect_with(
            "spoolframe-transcode-lacks-auto",
            HardwarePolicy::Auto,
            PASCAL,
        )
        .await;
        assert_eq!(lines(&ring, LogLevel::Warn, ""), Vec::<String>::new());
        assert_eq!(lines(&ring, LogLevel::Info, "lacks").len(), 1);
        assert_eq!(lines(&ring, LogLevel::Debug, "No capable devices").len(), 1);
    }

    /// A pick that finds no card is how the host is: a line. A device
    /// named and not there is a mistake: a warning.
    #[tokio::test]
    async fn a_pick_finding_nothing_is_a_line_and_a_missing_device_a_warning() {
        let (ring, detected) = detect_on(
            "spoolframe-transcode-missing",
            vec![
                GpuEntry::all(Hardware::Nvenc),
                GpuEntry {
                    api: Hardware::Vaapi,
                    which: Which::Device("/dev/dri/renderD199".into()),
                    max_jobs: None,
                },
            ],
            Vec::new(),
            NO_CUDA,
        )
        .await;
        assert!(detected.gpus.is_empty());
        let info = lines(&ring, LogLevel::Info, "takes no GPU");
        assert_eq!(info.len(), 1, "{info:?}");
        assert!(info[0].contains("entry 1 (nvenc pick=all)"), "{}", info[0]);
        assert!(info[0].contains("no NVIDIA card here"), "{}", info[0]);
        let warned = lines(&ring, LogLevel::Warn, "");
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].contains(
                "entry 2 (vaapi device=/dev/dri/renderD199) names a GPU that is not here"
            ),
            "{}",
            warned[0]
        );
    }

    /// The Server page's other GPUs: a card no entry takes, and one taken
    /// whose every test failed, with the first; and an entry that took
    /// none, as the log says it.
    #[tokio::test]
    async fn detection_keeps_the_cards_that_do_nothing_and_why() {
        let (_, untaken) = detect_on(
            "spoolframe-transcode-untaken",
            vec![GpuEntry::all(Hardware::Vaapi)],
            vec![geforce()],
            NO_CUDA,
        )
        .await;
        assert_eq!(untaken.others.len(), 1);
        assert_eq!(untaken.others[0].host.pci, geforce().pci);
        assert_eq!(untaken.others[0].why, OtherWhy::NoEntry);
        assert_eq!(untaken.notes.len(), 1, "{:?}", untaken.notes);
        assert!(
            untaken.notes[0].starts_with("transcode.gpus entry 1 (vaapi pick=all) takes no GPU: "),
            "{}",
            untaken.notes[0]
        );

        // Without its driver: no NVENC encode and no NVDEC decode.
        const DEAD: &str = "#!/bin/sh\n\
             case \"$*\" in\n\
             *-encoders*) printf ' ------\\n V....D h264_nvenc NVIDIA\\n V....D libx264 x264\\n' ;;\n\
             *cuda*|*nvenc*) echo 'Cannot load libcuda.so.1' >&2; exit 1 ;;\n\
             esac\n";
        let (_, broken) = detect_on(
            "spoolframe-transcode-broken",
            vec![GpuEntry::all(Hardware::Nvenc)],
            vec![geforce()],
            DEAD,
        )
        .await;
        assert!(broken.gpus.is_empty());
        assert!(broken.notes.is_empty(), "{:?}", broken.notes);
        assert_eq!(
            broken.others.iter().map(|o| &o.why).collect::<Vec<_>>(),
            [&OtherWhy::NothingWorks {
                api: Hardware::Nvenc,
                entry: "transcode.gpus entry 1 (nvenc pick=all)".into(),
                test: Some("h264_nvenc".into()),
            }]
        );
    }

    /// Two AMD cards under one `pick = "all"`: each detected on its own
    /// node, the one doing more first, each with its own line; the
    /// encoders the planner sees are both cards' and software's.
    #[tokio::test]
    async fn two_cards_of_one_api_are_each_detected_and_ranked() {
        let (ring, detected) = detect_on(
            "spoolframe-transcode-two-amd",
            vec![GpuEntry {
                max_jobs: Some(1),
                ..GpuEntry::all(Hardware::Vaapi)
            }],
            vec![
                radeon("0000:6d:00.0", "/dev/dri/renderD129", Kind::Integrated),
                radeon("0000:03:00.0", "/dev/dri/renderD128", Kind::Discrete),
            ],
            TWO_AMD,
        )
        .await;
        let names: Vec<&str> = detected
            .gpus
            .iter()
            .map(|g| g.found.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "AMD Radeon RX 7900 XTX",
                "AMD Ryzen 9 7950X3D 16-Core Processor"
            ]
        );
        assert!(detected.gpus.iter().all(|g| g.max_jobs == Some(1)));
        let merged: Vec<&str> = detected
            .encoders
            .video
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(merged, ["h264_vaapi", "libx264", "hevc_vaapi"]);
        let said = lines(&ring, LogLevel::Info, "GPU AMD Radeon RX 7900 XTX");
        assert_eq!(said.len(), 1, "{said:?}");
        for part in [
            "(vaapi, /dev/dri/renderD128)",
            "discrete (amdgpu fusion flag: test)",
            "transcode.gpus entry 1 (vaapi pick=all max_jobs=1)",
            "up to 1 jobs",
            "encodes h264_vaapi, hevc_vaapi",
        ] {
            assert!(said[0].contains(part), "{part}: {}", said[0]);
        }
        // What each lacks, on a line of its own: the iGPU, HEVC.
        let lacks = lines(&ring, LogLevel::Info, "lacks");
        assert_eq!(lacks.len(), 2, "{lacks:?}");
        assert!(
            lacks[1].contains("vaapi lacks: hevc encode"),
            "{}",
            lacks[1]
        );
        assert!(lacks[1].contains("7950X3D"), "{}", lacks[1]);
        assert!(!lacks[0].contains("hevc encode"), "{}", lacks[0]);
    }

    /// `best` keeps the card that does more, whichever probed first.
    #[tokio::test]
    async fn best_takes_the_card_that_does_more() {
        let (_, detected) = detect_on(
            "spoolframe-transcode-best",
            vec![GpuEntry {
                which: Which::Pick(Pick::Best),
                ..GpuEntry::all(Hardware::Vaapi)
            }],
            vec![
                radeon("0000:03:00.0", "/dev/dri/renderD129", Kind::Discrete),
                radeon("0000:6d:00.0", "/dev/dri/renderD128", Kind::Integrated),
            ],
            TWO_AMD,
        )
        .await;
        let nodes: Vec<PathBuf> = detected.gpus.iter().map(|g| g.vaapi_device()).collect();
        // renderD128 encodes HEVC too: more capable, integrated or not.
        assert_eq!(nodes, [PathBuf::from("/dev/dri/renderD128")]);
    }

    /// Two AMD cards, libplacebo's tone map (PGPU-04) working on
    /// renderD128 alone, or on `neither`.
    fn placebo_on_one(neither: bool) -> String {
        let refuse = if neither {
            "*libplacebo*"
        } else {
            "*renderD129*libplacebo*"
        };
        format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *-encoders*) printf ' ------\\n V....D h264_vaapi VAAPI\\n V....D libx264 x264\\n' ;;\n\
             {refuse}) echo 'no Vulkan device' >&2; exit 1 ;;\n\
             *tonemap_vaapi*|*overlay_vaapi*) exit 1 ;;\n\
             *-hwaccel*) exit 1 ;;\n\
             esac\n"
        )
    }

    /// PGPU-04 with PGPU-05: each card's libplacebo test is its own; the
    /// merged detection (the status, the planner) has VA-API's libplacebo
    /// from the card that runs it, whichever card is first; a card whose
    /// test passed counts the tone map in its capability; and a forced
    /// `tone_map = "libplacebo"` is not warned of while one card runs it.
    #[tokio::test]
    async fn each_card_tone_maps_with_libplacebo_as_its_own_test_says() {
        use spoolframe_media::encoders::{ToneMapSetting, VulkanDevice};
        let settings = TranscodeSettings {
            tone_map: ToneMapSetting::Libplacebo,
            ..TranscodeSettings::default()
        };
        let (ring, detected) = detect_on_with(
            "spoolframe-transcode-placebo-one",
            vec![GpuEntry::all(Hardware::Vaapi)],
            vec![
                radeon("0000:6d:00.0", "/dev/dri/renderD129", Kind::Integrated),
                radeon("0000:03:00.0", "/dev/dri/renderD128", Kind::Discrete),
            ],
            &placebo_on_one(false),
            settings.clone(),
        )
        .await;
        let by_node = |node: &str| {
            detected
                .gpus
                .iter()
                .find(|g| g.vaapi_device() == Path::new(node))
                .expect("detected")
                .clone()
        };
        let (dgpu, igpu) = (
            by_node("/dev/dri/renderD128"),
            by_node("/dev/dri/renderD129"),
        );
        assert_eq!(
            dgpu.encoders().libplacebo(Hardware::Vaapi),
            Some(VulkanDevice::Own)
        );
        assert_eq!(igpu.encoders().libplacebo(Hardware::Vaapi), None);
        assert_eq!(dgpu.found.capability, igpu.found.capability + 1);
        assert_eq!(
            detected.encoders.libplacebo(Hardware::Vaapi),
            Some(VulkanDevice::Own)
        );
        assert_eq!(
            lines(&ring, LogLevel::Warn, "tone_map"),
            Vec::<String>::new()
        );

        // libplacebo on neither: one warning, not one per card.
        let (ring, detected) = detect_on_with(
            "spoolframe-transcode-placebo-none",
            vec![GpuEntry::all(Hardware::Vaapi)],
            vec![
                radeon("0000:6d:00.0", "/dev/dri/renderD129", Kind::Integrated),
                radeon("0000:03:00.0", "/dev/dri/renderD128", Kind::Discrete),
            ],
            &placebo_on_one(true),
            settings,
        )
        .await;
        assert_eq!(detected.encoders.libplacebo(Hardware::Vaapi), None);
        let warned = lines(&ring, LogLevel::Warn, "forces a tone map");
        assert_eq!(warned.len(), 1, "{warned:?}");
    }
}
