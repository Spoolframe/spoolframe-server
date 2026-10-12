//! Which GPUs a server uses (PGPU-05): `transcode.gpus`, each entry an
//! API and the cards of it to take.
//!
//! Detection (`encoders::detect`) knows one card per API family. This
//! module wraps it: the host's cards are listed, each entry takes some of
//! them, and detection runs once per card taken, its result kept beside
//! the card.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::encoders::{
    self, Encoders, Ffmpeg, Hardware, HardwarePolicy, TestGpu, ToneMapSetting, TranscodeSettings,
};
use crate::sandbox::{GpuDevices, GpuUse};

/// Which cards of its API an entry takes (`pick = …`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Every card, the most capable first: a job goes to the next when one
    /// is full.
    All,
    /// The most capable card alone.
    Best,
    /// Discrete cards only, the most capable first.
    Dgpu,
    /// Integrated ones only, the most capable first.
    Igpu,
}

impl Pick {
    pub fn as_str(self) -> &'static str {
        match self {
            Pick::All => "all",
            Pick::Best => "best",
            Pick::Dgpu => "dgpu",
            Pick::Igpu => "igpu",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        [Pick::All, Pick::Best, Pick::Dgpu, Pick::Igpu]
            .into_iter()
            .find(|p| p.as_str() == word)
    }
}

/// How an entry names its cards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    /// Picked from the cards found.
    Pick(Pick),
    /// Exactly this one: a CUDA index for NVIDIA (`0`), a render node for
    /// VA-API and QSV (`/dev/dri/by-path/pci-0000:03:00.0-render`).
    Device(String),
}

/// One entry of `transcode.gpus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEntry {
    pub api: Hardware,
    pub which: Which,
    /// GPU video encodes at once on each of its cards: `max_jobs`, else
    /// `transcode.max_gpu_jobs`.
    pub max_jobs: Option<usize>,
}

impl GpuEntry {
    /// Every card of `api`.
    pub fn all(api: Hardware) -> Self {
        Self {
            api,
            which: Which::Pick(Pick::All),
            max_jobs: None,
        }
    }

    /// `transcode.gpus` left out: every card of every API, NVIDIA first.
    pub fn defaults() -> Vec<GpuEntry> {
        [Hardware::Nvenc, Hardware::Vaapi, Hardware::Qsv]
            .into_iter()
            .map(Self::all)
            .collect()
    }

    /// The entries the old `transcode.hardware` and `vaapi_device` mean:
    /// each family every card of its API, in the order named, but VA-API
    /// (and QSV, which goes through VA-API's render node) on
    /// `vaapi_device` alone when that was set. `auto` is the default list;
    /// `none` no GPU.
    pub fn from_hardware(policy: &HardwarePolicy, vaapi_device: Option<&Path>) -> Vec<GpuEntry> {
        let families = match policy {
            HardwarePolicy::Auto => GpuEntry::defaults().into_iter().map(|e| e.api).collect(),
            HardwarePolicy::Ordered(order) => order.clone(),
            HardwarePolicy::None => Vec::new(),
        };
        families
            .into_iter()
            .map(|api| match (api, vaapi_device) {
                (Hardware::Vaapi | Hardware::Qsv, Some(node)) => GpuEntry {
                    api,
                    which: Which::Device(node.display().to_string()),
                    max_jobs: None,
                },
                _ => GpuEntry::all(api),
            })
            .collect()
    }

    /// The entry as `transcode.gpus` writes it: `{ api = "vaapi"; pick =
    /// "all"; }`.
    pub fn ucl(&self) -> String {
        let which = match &self.which {
            Which::Pick(pick) => format!("pick = \"{}\";", pick.as_str()),
            Which::Device(device) => format!("device = {device:?};"),
        };
        let max = self
            .max_jobs
            .map(|n| format!(" max_jobs = {n};"))
            .unwrap_or_default();
        format!("{{ api = \"{}\"; {which}{max} }}", self.api.as_str())
    }

    /// `entries` as `transcode.gpus = …` takes them: what to write in
    /// place of the deprecated `hardware` and `vaapi_device`.
    pub fn ucl_list(entries: &[GpuEntry]) -> String {
        let each: Vec<String> = entries.iter().map(GpuEntry::ucl).collect();
        format!("[{}]", each.join(", "))
    }

    /// For the log: `vaapi pick=dgpu`, `nvenc device=1 max_jobs=2`.
    pub fn describe(&self) -> String {
        let mut text = match &self.which {
            Which::Pick(pick) => format!("{} pick={}", self.api.as_str(), pick.as_str()),
            Which::Device(device) => format!("{} device={device}", self.api.as_str()),
        };
        if let Some(max) = self.max_jobs {
            text.push_str(&format!(" max_jobs={max}"));
        }
        text
    }
}

/// Whether a card is a card of its own or part of the CPU's package.
/// Discrete first: a discrete card ranks before an integrated one that
/// does as much.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Discrete,
    Integrated,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Discrete => "discrete",
            Kind::Integrated => "integrated",
        }
    }
}

/// A card's [`Kind`], and how that was known: for the log, and so a
/// guess reads as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub kind: Kind,
    pub how: String,
}

/// Discrete or integrated, from what the kernel says of the card.
///
/// Vulkan's device type is the authoritative answer (it says discrete or
/// integrated, and `VK_EXT_physical_device_drm` ties it to the render
/// node). ffmpeg has Vulkan now (PGPU-04), but its listing at `-v
/// verbose` names each device's type and PCI device id, not its node,
/// and cards are classified here, before any ffmpeg runs (`select` reads
/// the kind). Asking it would take a run per card confined to that
/// card's node, ahead of `select`; not done.
///
/// - NVIDIA: discrete.
/// - AMD (`amdgpu`): the driver's own answer, `fusion` (the
///   `AMDGPU_IDS_FLAGS_FUSION` bit of `AMDGPU_INFO_DEV_INFO`): an APU's
///   GPU is integrated. `None` or an error when it could not be asked.
/// - Intel: its iGPU is always PCI device 00:02.0, on the root bus; a
///   discrete Arc is behind a PCIe port, on a bus of its own.
/// - Anything else is a guess, integrated.
pub fn classify(
    vendor: u16,
    driver: &str,
    pci: &str,
    fusion: Option<Result<bool, String>>,
) -> Class {
    let class = |kind, how: String| Class { kind, how };
    if driver == "nvidia" || vendor == NVIDIA {
        return class(Kind::Discrete, "NVIDIA: every NVIDIA card is".into());
    }
    if driver == "amdgpu" {
        return match fusion {
            Some(Ok(true)) => class(Kind::Integrated, "amdgpu fusion flag: an APU".into()),
            Some(Ok(false)) => class(Kind::Discrete, "amdgpu fusion flag: not an APU".into()),
            Some(Err(why)) => class(
                Kind::Discrete,
                format!("guess: the amdgpu info ioctl failed ({why})"),
            ),
            None => class(
                Kind::Discrete,
                "guess: amdgpu, with no render node to ask".into(),
            ),
        };
    }
    if vendor == INTEL {
        return if pci.ends_with(":00:02.0") {
            class(
                Kind::Integrated,
                "Intel at PCI 00:02.0, the iGPU's place".into(),
            )
        } else {
            class(Kind::Discrete, "Intel, not at PCI 00:02.0".into())
        };
    }
    class(
        Kind::Integrated,
        format!("guess: driver {driver:?}, neither AMD, Intel nor NVIDIA"),
    )
}

const NVIDIA: u16 = 0x10de;
const INTEL: u16 = 0x8086;
const AMD: u16 = 0x1002;

/// What `AMDGPU_INFO_DEV_INFO` says of a card, of what this needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmdgpuDevInfo {
    /// Its PCI device id, as sysfs has it too.
    pub device_id: u32,
    /// An APU: the GPU is part of the CPU's package.
    pub fusion: bool,
}

/// `struct drm_amdgpu_info_device`'s `ids_flags`, from its start: eight
/// `__u32`s, two `__u64` clocks, two `__u32`s, `cu_bitmap[4][4]` and four
/// `__u32`s before it (include/uapi/drm/amdgpu_drm.h).
const IDS_FLAGS_AT: usize = 136;
/// `AMDGPU_IDS_FLAGS_FUSION`.
const IDS_FLAGS_FUSION: u64 = 0x1;

impl AmdgpuDevInfo {
    /// From the bytes the kernel wrote: `None` if too few.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let device_id = u32::from_ne_bytes(bytes.get(0..4)?.try_into().ok()?);
        let flags = u64::from_ne_bytes(bytes.get(IDS_FLAGS_AT..IDS_FLAGS_AT + 8)?.try_into().ok()?);
        Some(Self {
            device_id,
            fusion: flags & IDS_FLAGS_FUSION != 0,
        })
    }

    /// Asks the amdgpu driver through `render`, a render node: the same
    /// `DRM_IOCTL_AMDGPU_INFO` Mesa and libdrm ask at start.
    pub fn read(render: &Path) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd as _;

        /// `struct drm_amdgpu_info`: 32 bytes, the largest member of its
        /// union 16.
        #[repr(C)]
        struct DrmAmdgpuInfo {
            return_pointer: u64,
            return_size: u32,
            query: u32,
            union: [u32; 4],
        }
        /// `DRM_IOW(DRM_COMMAND_BASE + DRM_AMDGPU_INFO, struct
        /// drm_amdgpu_info)`: `_IOW('d', 0x45, 32 bytes)`.
        const DRM_IOCTL_AMDGPU_INFO: libc::c_ulong = 0x4020_6445;
        const AMDGPU_INFO_DEV_INFO: u32 = 0x16;
        // Room for every field a newer kernel adds; it writes no more
        // than it is told there is.
        let mut out = [0u8; 512];
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(render)?;
        let request = DrmAmdgpuInfo {
            return_pointer: out.as_mut_ptr() as u64,
            return_size: out.len() as u32,
            query: AMDGPU_INFO_DEV_INFO,
            union: [0; 4],
        };
        // SAFETY: the request is a valid `drm_amdgpu_info` that the kernel
        // only reads; it writes at most `return_size` bytes to `out`, which
        // lives past the call.
        let done = unsafe { libc::ioctl(file.as_raw_fd(), DRM_IOCTL_AMDGPU_INFO as _, &request) };
        if done != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Self::decode(&out).ok_or_else(|| std::io::Error::other("a short answer"))
    }
}

/// An NVIDIA card as CUDA and its device node know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvidiaCard {
    /// Its CUDA index with `CUDA_DEVICE_ORDER=PCI_BUS_ID`: its place in
    /// PCI order among NVIDIA cards, as `nvidia-smi` numbers them.
    pub index: u32,
    /// Its `/dev/nvidia<minor>`: from `/proc/driver/nvidia`, or, where
    /// that is hidden (systemd's `ProcSubset=pid`), taken to be `index`:
    /// the driver numbers cards in the order it probes them, PCI order.
    pub minor: u32,
    /// The model `/proc/driver/nvidia` names, where it is readable.
    pub model: Option<String>,
}

/// A GPU of this host, as the kernel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCard {
    /// Its PCI address: `0000:03:00.0`.
    pub pci: String,
    pub vendor: u16,
    pub device_id: u16,
    /// The kernel driver bound to it: `amdgpu`, `i915`, `xe`, `nvidia`.
    pub driver: String,
    /// Its render node, `/dev/dri/renderD128`, if it has one.
    pub render: Option<PathBuf>,
    /// The same node by PCI path, which does not change between boots.
    pub by_path: Option<PathBuf>,
    pub nvidia: Option<NvidiaCard>,
    pub class: Class,
}

impl HostCard {
    /// A name when ffmpeg gave none: NVIDIA's model, else the vendor and
    /// PCI ids, `AMD 1002:744c`.
    pub fn fallback_name(&self) -> String {
        if let Some(model) = self.nvidia.as_ref().and_then(|n| n.model.clone()) {
            return model;
        }
        let vendor = match self.vendor {
            AMD => "AMD",
            INTEL => "Intel",
            NVIDIA => "NVIDIA",
            _ => "GPU",
        };
        format!("{vendor} {:04x}:{:04x}", self.vendor, self.device_id)
    }
}

/// Where the kernel's lists are: `/sys`, `/dev`, `/proc` on a host; a
/// made-up tree in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRoots {
    pub sys: PathBuf,
    pub dev: PathBuf,
    pub proc: PathBuf,
}

impl Default for HostRoots {
    fn default() -> Self {
        Self {
            sys: PathBuf::from("/sys"),
            dev: PathBuf::from("/dev"),
            proc: PathBuf::from("/proc"),
        }
    }
}

/// Every GPU this host has, in PCI order: each DRM render node's device,
/// and each PCI device the `nvidia` driver is bound to (one with
/// `nvidia-drm` has a render node too, and is one card).
pub fn host_cards(roots: &HostRoots) -> Vec<HostCard> {
    let mut cards: Vec<HostCard> = Vec::new();
    let mut renders: Vec<(u32, String)> = std::fs::read_dir(roots.sys.join("class/drm"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let n = name.strip_prefix("renderD")?.parse().ok()?;
            Some((n, name))
        })
        .collect();
    renders.sort();
    for (_, name) in renders {
        let Ok(device) =
            std::fs::canonicalize(roots.sys.join("class/drm").join(&name).join("device"))
        else {
            continue;
        };
        let Some(pci) = pci_address(&device) else {
            // A virtual device (vgem, a software renderer): no GPU.
            continue;
        };
        let by_path = roots.dev.join(format!("dri/by-path/pci-{pci}-render"));
        let (vendor, device_id) = pci_ids(&device);
        cards.push(HostCard {
            render: Some(roots.dev.join("dri").join(&name)),
            by_path: by_path.exists().then_some(by_path),
            vendor,
            device_id,
            driver: driver(&device),
            nvidia: None,
            class: classify(0, "", "", None),
            pci,
        });
    }
    let mut nvidia: Vec<String> = std::fs::read_dir(roots.sys.join("bus/pci/drivers/nvidia"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| pci_address(&entry.path()))
        .collect();
    nvidia.sort();
    for (index, pci) in nvidia.into_iter().enumerate() {
        let index = index as u32;
        let info = std::fs::read_to_string(
            roots
                .proc
                .join("driver/nvidia/gpus")
                .join(&pci)
                .join("information"),
        )
        .unwrap_or_default();
        let field = |name: &str| {
            info.lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|rest| rest.trim_start_matches(':').trim().to_owned())
        };
        let minor = field("Device Minor").and_then(|m| m.parse().ok());
        if minor.is_none() {
            tracing::debug!(
                pci,
                index,
                "/proc/driver/nvidia is not readable: the card's device node is taken to be \
                 /dev/nvidia{index}, in PCI order"
            );
        }
        let card = NvidiaCard {
            index,
            minor: minor.unwrap_or(index),
            model: field("Model").filter(|m| !m.is_empty()),
        };
        // Its runs are let open this node alone: one that is not there
        // (a guess, above, that was wrong) fails them later inside CUDA,
        // with nothing to say why (review 2026-10-11 L64).
        let node = roots.dev.join(format!("nvidia{}", card.minor));
        if !node.exists() {
            tracing::warn!(
                pci,
                index,
                node = %node.display(),
                guessed = minor.is_none(),
                "the NVIDIA card's device node is missing: its encodes will fail"
            );
        }
        match cards.iter_mut().find(|c| c.pci == pci) {
            Some(known) => known.nvidia = Some(card),
            None => {
                let device = roots.sys.join("bus/pci/devices").join(&pci);
                let (vendor, device_id) = pci_ids(&device);
                cards.push(HostCard {
                    pci,
                    vendor,
                    device_id,
                    driver: "nvidia".into(),
                    render: None,
                    by_path: None,
                    nvidia: Some(card),
                    class: classify(0, "", "", None),
                });
            }
        }
    }
    for card in &mut cards {
        let fusion = (card.driver == "amdgpu").then(|| {
            card.render.as_deref().map(|node| {
                AmdgpuDevInfo::read(node)
                    .map(|i| i.fusion)
                    .map_err(|e| e.to_string())
            })
        });
        card.class = classify(card.vendor, &card.driver, &card.pci, fusion.flatten());
    }
    cards.sort_by(|a, b| a.pci.cmp(&b.pci));
    cards
}

/// `0000:03:00.0` from a device's sysfs path, if it is a PCI device's.
fn pci_address(device: &Path) -> Option<String> {
    let name = device.file_name()?.to_str()?;
    let bytes = name.as_bytes();
    let pci = bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| [4, 7, 10].contains(&i) || c.is_ascii_hexdigit());
    pci.then(|| name.to_owned())
}

/// A PCI device's vendor and device ids, 0 where unreadable.
fn pci_ids(device: &Path) -> (u16, u16) {
    let read = |file: &str| {
        std::fs::read_to_string(device.join(file))
            .ok()
            .and_then(|t| u16::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
            .unwrap_or(0)
    };
    (read("vendor"), read("device"))
}

/// The driver bound to a device: its `driver` link's name.
fn driver(device: &Path) -> String {
    std::fs::read_link(device.join("driver"))
        .ok()
        .and_then(|link| Some(link.file_name()?.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// A card an entry of `transcode.gpus` took, for that entry's API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub api: Hardware,
    pub host: HostCard,
    /// The entry, by its place in `transcode.gpus`.
    pub entry: usize,
}

impl Card {
    /// Where it is, for the log and the budget: the render node by PCI
    /// path where there is one, else the node, else `cuda:<index>`.
    pub fn place(&self) -> String {
        match (
            self.api,
            &self.host.by_path,
            &self.host.render,
            &self.host.nvidia,
        ) {
            (Hardware::Nvenc, _, _, Some(nvidia)) => {
                format!("cuda:{} ({})", nvidia.index, self.host.pci)
            }
            (_, Some(by_path), _, _) => by_path.display().to_string(),
            (_, None, Some(render), _) => render.display().to_string(),
            _ => self.host.pci.clone(),
        }
    }

    /// One key a card, by API: the same card under VA-API and QSV is two
    /// (two entries, two detections), as its jobs' budget is.
    pub fn key(&self) -> String {
        format!("{}:{}", self.api.as_str(), self.host.pci)
    }

    /// The render node a VA-API or QSV run names; for an NVIDIA card,
    /// nothing a run uses.
    pub fn vaapi_device(&self) -> PathBuf {
        self.host
            .render
            .clone()
            .filter(|_| self.api != Hardware::Nvenc)
            .unwrap_or_default()
    }

    /// What detection tests: this card's API alone, on its node, with
    /// the rest of `base` (the tone map and experimental switches,
    /// PGPU-04) as the server has them.
    pub fn settings(&self, base: &TranscodeSettings) -> TranscodeSettings {
        TranscodeSettings {
            hardware: HardwarePolicy::Ordered(vec![self.api]),
            vaapi_device: self.vaapi_device(),
            ..base.clone()
        }
    }

    /// The device nodes its runs may open, of `dev`, and no other card's.
    pub fn devices(&self, dev: &Path) -> GpuDevices {
        match (self.api, &self.host.nvidia, &self.host.render) {
            (Hardware::Nvenc, Some(nvidia), _) => {
                GpuDevices::nvidia_card(dev, nvidia.minor, nvidia.index)
            }
            (Hardware::Vaapi | Hardware::Qsv, _, Some(render)) => GpuDevices::vaapi_card(render),
            _ => GpuDevices::default(),
        }
    }
}

/// An entry that took no card, and why: for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub entry: usize,
    /// It named one `device`: that it is missing is a warning. A pick
    /// that finds none is how the host is.
    pub fixed: bool,
    pub why: String,
}

/// The cards each entry takes of `host`, in the entries' order, each in
/// PCI order within its entry (ranked once detected: [`rank`]). A card an
/// earlier entry took for the same API is not taken again. A pick takes
/// NVIDIA's cards for `nvenc`, the other render nodes for `vaapi`, and
/// Intel's for `qsv` (Quick Sync is Intel's alone); `dgpu` and `igpu` by
/// [`classify`]. A `device` takes the card it names, whatever it is.
pub fn select(entries: &[GpuEntry], host: &[HostCard]) -> (Vec<Card>, Vec<Missing>) {
    let mut cards: Vec<Card> = Vec::new();
    let mut missing = Vec::new();
    for (n, entry) in entries.iter().enumerate() {
        let taken = |card: &HostCard| {
            cards
                .iter()
                .any(|c| c.api == entry.api && c.host.pci == card.pci)
        };
        let of_api = |card: &HostCard| match entry.api {
            Hardware::Nvenc => card.nvidia.is_some(),
            Hardware::Vaapi => card.render.is_some() && card.driver != "nvidia",
            Hardware::Qsv => card.render.is_some() && card.vendor == INTEL,
        };
        let found: Vec<&HostCard> = match &entry.which {
            Which::Pick(pick) => {
                let found: Vec<&HostCard> = host
                    .iter()
                    .filter(|c| of_api(c))
                    .filter(|c| match pick {
                        Pick::All | Pick::Best => true,
                        Pick::Dgpu => c.class.kind == Kind::Discrete,
                        Pick::Igpu => c.class.kind == Kind::Integrated,
                    })
                    .collect();
                if found.is_empty() {
                    let api = match entry.api {
                        Hardware::Nvenc => "NVIDIA",
                        Hardware::Vaapi => "VA-API",
                        Hardware::Qsv => "Intel (Quick Sync)",
                    };
                    let what = match pick {
                        Pick::Dgpu => " discrete",
                        Pick::Igpu => " integrated",
                        Pick::All | Pick::Best => "",
                    };
                    missing.push(Missing {
                        entry: n,
                        fixed: false,
                        why: format!("no{what} {api} card here"),
                    });
                }
                found
            }
            Which::Device(device) => {
                let found = match entry.api {
                    Hardware::Nvenc => host.iter().find(|c| {
                        c.nvidia
                            .as_ref()
                            .is_some_and(|nv| nv.index.to_string() == *device)
                    }),
                    Hardware::Vaapi | Hardware::Qsv => {
                        let wanted = std::fs::canonicalize(device).ok();
                        host.iter().find(|c| {
                            wanted.is_some()
                                && c.render
                                    .as_deref()
                                    .and_then(|r| std::fs::canonicalize(r).ok())
                                    == wanted
                        })
                    }
                };
                if found.is_none() {
                    let nvidia = host.iter().filter(|c| c.nvidia.is_some()).count();
                    missing.push(Missing {
                        entry: n,
                        fixed: true,
                        why: match entry.api {
                            Hardware::Nvenc => {
                                format!("no NVIDIA card has CUDA index {device}: {nvidia} found")
                            }
                            _ => format!("{device} is not a GPU's render node here"),
                        },
                    });
                }
                found.into_iter().collect()
            }
        };
        let fresh: Vec<Card> = found
            .into_iter()
            .filter(|c| !taken(c))
            .map(|host| Card {
                api: entry.api,
                host: host.clone(),
                entry: n,
            })
            .collect();
        cards.extend(fresh);
    }
    (cards, missing)
}

/// How much of a job `encoders`, one card's detection, says the card
/// does on `api`: each codec it encodes, each codec and depth it decodes,
/// and each filter it runs on the GPU, one each.
pub fn capability(encoders: &Encoders, api: Hardware) -> usize {
    let encodes = encoders
        .video
        .iter()
        .filter(|e| e.hardware == Some(api))
        .count();
    let decodes = encoders
        .hardware_decoders
        .iter()
        .filter(|d| d.hardware == api)
        .count();
    // A tone map on the GPU counts once, its driver's own
    // (`tonemap_vaapi`) or libplacebo's (PGPU-04): a card whose libplacebo
    // test passed tone maps on the GPU as one with `tonemap_vaapi` does.
    let tone_map = !encoders.gpu_tone_mappers(api).is_empty();
    let filters = match api {
        Hardware::Vaapi => [tone_map, encoders.gpu_overlay, false],
        Hardware::Nvenc => [tone_map, encoders.cuda_scale, encoders.cuda_overlay],
        Hardware::Qsv => [false, false, false],
    };
    encodes + decodes + filters.into_iter().filter(|f| *f).count()
}

/// A card detection ran on, and what it found.
#[derive(Debug, Clone)]
pub struct Found {
    pub card: Card,
    /// Human: the driver's name for it (`AMD Radeon RX 7900 XTX`), else
    /// [`HostCard::fallback_name`].
    pub name: String,
    /// Detection's result with this card the only one of its API.
    pub encoders: Encoders,
    /// [`capability`].
    pub capability: usize,
}

/// The cards found, in the order jobs try them: by entry, and within one
/// the most capable first, discrete before integrated among equals, then
/// the lower PCI address (for NVIDIA, the lower CUDA index). A `best`
/// entry keeps its first card alone.
pub fn rank(mut found: Vec<Found>, entries: &[GpuEntry]) -> Vec<Found> {
    found.sort_by(|a, b| {
        (
            a.card.entry,
            std::cmp::Reverse(a.capability),
            a.card.host.class.kind,
            &a.card.host.pci,
        )
            .cmp(&(
                b.card.entry,
                std::cmp::Reverse(b.capability),
                b.card.host.class.kind,
                &b.card.host.pci,
            ))
    });
    let mut kept: Vec<Found> = Vec::new();
    for f in found {
        let best = entries
            .get(f.card.entry)
            .is_some_and(|e| e.which == Which::Pick(Pick::Best));
        if best && kept.iter().any(|k| k.card.entry == f.card.entry) {
            continue;
        }
        kept.push(f);
    }
    kept
}

/// Detection on each card in `cards`, with the ffmpeg `ffmpeg_for` gives
/// for it (confined to that card's nodes), and once with `software` for
/// the software encoders the server has whatever its GPUs: what the
/// cards' own detections then find again without running it. `base`
/// carries what is not a card's own (`tone_map`, the experimental
/// switches).
pub async fn detect_cards(
    software: &dyn Ffmpeg,
    ffmpeg_for: &(dyn Fn(&Card) -> Box<dyn Ffmpeg> + Sync),
    cards: Vec<Card>,
    base: &TranscodeSettings,
    scratch: &Path,
) -> (Encoders, Vec<Found>) {
    let memo = Memo::default();
    // Whether a forced `tone_map` runs on no GPU is a question of all the
    // cards together: the caller asks it of their merged detection. Each
    // card's own would warn for every card that lacks it, and software's
    // always.
    let base = &TranscodeSettings {
        tone_map: ToneMapSetting::Auto,
        ..base.clone()
    };
    let none = TranscodeSettings {
        hardware: HardwarePolicy::None,
        ..base.clone()
    };
    let baseline = encoders::detect(
        &Remembered {
            inner: software,
            memo: &memo,
        },
        &none,
        scratch,
    )
    .await;
    let mut found = Vec::new();
    for card in cards {
        let ffmpeg = ffmpeg_for(&card);
        let ffmpeg = Remembered {
            inner: ffmpeg.as_ref(),
            memo: &memo,
        };
        let encoders = encoders::detect(&ffmpeg, &card.settings(base), scratch).await;
        let name = card_name(&ffmpeg, &card, scratch)
            .await
            .unwrap_or_else(|| card.host.fallback_name());
        found.push(Found {
            capability: capability(&encoders, card.api),
            card,
            name,
            encoders,
        });
    }
    (baseline, found)
}

/// The driver's name for `card`, from ffmpeg at `-v verbose`: VA-API's
/// driver string, or the CUDA device NVENC opens.
async fn card_name(ffmpeg: &dyn Ffmpeg, card: &Card, scratch: &Path) -> Option<String> {
    let mut args: Vec<String> = ["-hide_banner", "-v", "verbose"].map(String::from).to_vec();
    match card.api {
        Hardware::Vaapi | Hardware::Qsv => args.extend([
            "-init_hw_device".to_owned(),
            format!("vaapi=va:{}", card.vaapi_device().display()),
        ]),
        Hardware::Nvenc => {}
    }
    args.extend(["-f", "lavfi", "-i", "nullsrc=s=256x256:d=0.04"].map(String::from));
    if card.api == Hardware::Nvenc {
        args.extend(["-c:v", "h264_nvenc"].map(String::from));
    }
    args.extend(["-f", "null", "-"].map(String::from));
    let said = ffmpeg.stderr(&args, scratch, card.api.gpu()).await?;
    match card.api {
        Hardware::Nvenc => nvenc_name(&said),
        Hardware::Vaapi | Hardware::Qsv => vaapi_name(&said),
    }
}

/// The card in VA-API's `VAAPI driver: …` line: Mesa's `… for AMD
/// Radeon RX 7900 XTX (radeonsi, navi31, …).` is the part after `for`.
pub fn vaapi_name(stderr: &str) -> Option<String> {
    let driver = stderr
        .lines()
        .find_map(|line| line.split_once("VAAPI driver: ").map(|(_, d)| d.trim()))?;
    let driver = driver.trim_end_matches('.');
    let named = driver.split_once(" for ").map_or(driver, |(_, card)| card);
    let named = named
        .split_once(" (")
        .map_or(named, |(card, _)| card)
        .trim();
    (!named.is_empty()).then(|| named.to_owned())
}

/// The card in NVENC's `[ GPU #0 - < NVIDIA GeForce GTX 1060 6GB > has
/// Compute SM 6.1 ]`.
pub fn nvenc_name(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        let (_, rest) = line.split_once("< ")?;
        let (name, _) = rest.split_once(" > has Compute")?;
        Some(name.trim().to_owned()).filter(|n| !n.is_empty())
    })
}

/// What runs of no GPU gave that are the same for every card: the
/// software encoders' tests, and the samples they write into the shared
/// scratch.
#[derive(Default)]
struct Memo {
    runs: Mutex<HashMap<Vec<String>, Result<(), String>>>,
    list: Mutex<Option<Result<String, String>>>,
}

/// An ffmpeg that runs a software test once for every card.
struct Remembered<'a> {
    inner: &'a dyn Ffmpeg,
    memo: &'a Memo,
}

#[async_trait]
impl Ffmpeg for Remembered<'_> {
    async fn encoders(&self) -> Result<String, String> {
        if let Some(list) = self.memo.list.lock().clone() {
            return list;
        }
        let list = self.inner.encoders().await;
        *self.memo.list.lock() = Some(list.clone());
        list
    }

    async fn succeeds(&self, args: &[String], scratch: &Path, gpu: TestGpu) -> Result<(), String> {
        // A software sample writes its file into the scratch every card
        // shares: remembered too, while the file it wrote is there, so the
        // decoders' samples are encoded once and not once a card (review
        // 2026-10-11 L62).
        let writes = args
            .last()
            .map(Path::new)
            .filter(|out| out.starts_with(scratch));
        let same_everywhere = gpu.gpu == GpuUse::None
            && gpu.vulkan.is_none()
            && (args.ends_with(&["-f".into(), "null".into(), "-".into()]) || writes.is_some());
        if !same_everywhere {
            return self.inner.succeeds(args, scratch, gpu).await;
        }
        if let Some(known) = self.memo.runs.lock().get(args).cloned()
            && (known.is_err() || writes.is_none_or(Path::exists))
        {
            return known;
        }
        let ran = self.inner.succeeds(args, scratch, gpu).await;
        self.memo.runs.lock().insert(args.to_vec(), ran.clone());
        ran
    }

    async fn stderr(&self, args: &[String], scratch: &Path, gpu: GpuUse) -> Option<String> {
        self.inner.stderr(args, scratch, gpu).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_every_card_of_every_api_nvidia_first() {
        let apis: Vec<Hardware> = GpuEntry::defaults().iter().map(|e| e.api).collect();
        assert_eq!(apis, [Hardware::Nvenc, Hardware::Vaapi, Hardware::Qsv]);
        assert!(
            GpuEntry::defaults()
                .iter()
                .all(|e| e.which == Which::Pick(Pick::All) && e.max_jobs.is_none())
        );
    }

    #[test]
    fn a_named_family_is_every_card_of_it_in_the_order_named() {
        let entries = GpuEntry::from_hardware(
            &HardwarePolicy::Ordered(vec![Hardware::Vaapi, Hardware::Nvenc]),
            None,
        );
        assert_eq!(
            entries,
            [
                GpuEntry::all(Hardware::Vaapi),
                GpuEntry::all(Hardware::Nvenc)
            ]
        );
    }

    #[test]
    fn vaapi_device_is_the_one_vaapi_card() {
        let node = Path::new("/dev/dri/renderD129");
        let entries = GpuEntry::from_hardware(
            &HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi]),
            Some(node),
        );
        assert_eq!(entries[0], GpuEntry::all(Hardware::Nvenc));
        assert_eq!(
            entries[1].which,
            Which::Device("/dev/dri/renderD129".into())
        );
        // `auto` with a render node set: that node for VA-API and QSV.
        let auto = GpuEntry::from_hardware(&HardwarePolicy::Auto, Some(node));
        let devices: Vec<&Which> = auto.iter().map(|e| &e.which).collect();
        assert_eq!(
            devices,
            [
                &Which::Pick(Pick::All),
                &Which::Device("/dev/dri/renderD129".into()),
                &Which::Device("/dev/dri/renderD129".into()),
            ]
        );
    }

    #[test]
    fn none_is_no_gpu_and_auto_the_default() {
        assert!(GpuEntry::from_hardware(&HardwarePolicy::None, None).is_empty());
        assert_eq!(
            GpuEntry::from_hardware(&HardwarePolicy::Auto, None),
            GpuEntry::defaults()
        );
    }

    #[test]
    fn an_entry_describes_itself_as_written() {
        let entry = GpuEntry {
            api: Hardware::Nvenc,
            which: Which::Device("1".into()),
            max_jobs: Some(2),
        };
        assert_eq!(entry.describe(), "nvenc device=1 max_jobs=2");
        assert_eq!(
            GpuEntry {
                which: Which::Pick(Pick::Dgpu),
                ..GpuEntry::all(Hardware::Vaapi)
            }
            .describe(),
            "vaapi pick=dgpu"
        );
    }

    #[test]
    fn entries_are_written_as_gpus_takes_them() {
        let entries = GpuEntry::from_hardware(
            &HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi]),
            Some(Path::new("/dev/dri/renderD128")),
        );
        assert_eq!(
            GpuEntry::ucl_list(&entries),
            r#"[{ api = "nvenc"; pick = "all"; }, { api = "vaapi"; device = "/dev/dri/renderD128"; }]"#
        );
        assert_eq!(GpuEntry::ucl_list(&[]), "[]");
        let limited = GpuEntry {
            max_jobs: Some(1),
            ..GpuEntry::all(Hardware::Qsv)
        };
        assert_eq!(
            limited.ucl(),
            r#"{ api = "qsv"; pick = "all"; max_jobs = 1; }"#
        );
    }

    // ---- The host's cards

    /// A made-up host: sysfs, /dev and /proc under one directory.
    struct FakeHost {
        _dir: spoolframe_test_dir::TestDir,
        roots: HostRoots,
    }

    impl FakeHost {
        async fn new(name: &str) -> Self {
            let dir = spoolframe_test_dir::TestDir::new(&format!("spoolframe-gpus-{name}")).await;
            let roots = HostRoots {
                sys: dir.join("sys"),
                dev: dir.join("dev"),
                proc: dir.join("proc"),
            };
            for d in [
                roots.sys.join("class/drm"),
                roots.sys.join("bus/pci/drivers/nvidia"),
                roots.sys.join("bus/pci/devices"),
                roots.dev.join("dri/by-path"),
            ] {
                std::fs::create_dir_all(d).unwrap();
            }
            Self { _dir: dir, roots }
        }

        /// A PCI device with `driver` bound.
        fn device(&self, pci: &str, vendor: u16, device: u16, driver: &str) -> PathBuf {
            let path = self.roots.sys.join("devices/pci0000:00").join(pci);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("vendor"), format!("0x{vendor:04x}\n")).unwrap();
            std::fs::write(path.join("device"), format!("0x{device:04x}\n")).unwrap();
            let drivers = self.roots.sys.join("bus/pci/drivers").join(driver);
            std::fs::create_dir_all(&drivers).unwrap();
            std::os::unix::fs::symlink(&drivers, path.join("driver")).unwrap();
            std::os::unix::fs::symlink(&path, self.roots.sys.join("bus/pci/devices").join(pci))
                .unwrap();
            path
        }

        /// A card with a render node, and its by-path link.
        fn render(&self, n: u32, pci: &str, vendor: u16, device: u16, driver: &str) {
            let path = self.device(pci, vendor, device, driver);
            let class = self.roots.sys.join(format!("class/drm/renderD{n}"));
            std::fs::create_dir_all(&class).unwrap();
            std::os::unix::fs::symlink(&path, class.join("device")).unwrap();
            std::fs::write(self.roots.dev.join(format!("dri/renderD{n}")), "").unwrap();
            std::os::unix::fs::symlink(
                format!("../renderD{n}"),
                self.roots.dev.join(format!("dri/by-path/pci-{pci}-render")),
            )
            .unwrap();
        }

        /// An NVIDIA card bound to `nvidia`, with or without what
        /// /proc/driver/nvidia says of it.
        fn nvidia(&self, pci: &str, minor: Option<u32>, model: &str) {
            let path = self.device(pci, NVIDIA, 0x1c03, "nvidia");
            std::os::unix::fs::symlink(
                &path,
                self.roots.sys.join("bus/pci/drivers/nvidia").join(pci),
            )
            .unwrap();
            if let Some(minor) = minor {
                let info = self.roots.proc.join("driver/nvidia/gpus").join(pci);
                std::fs::create_dir_all(&info).unwrap();
                std::fs::write(
                    info.join("information"),
                    format!("Model: \t\t {model}\nIRQ:   \t\t 130\nBus Location: \t {pci}\nDevice Minor: \t {minor}\n"),
                )
                .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn every_render_node_and_nvidia_card_is_a_host_card_in_pci_order() {
        let host = FakeHost::new("enumerate").await;
        host.render(128, "0000:03:00.0", AMD, 0x744c, "amdgpu");
        host.render(129, "0000:00:02.0", INTEL, 0xa780, "i915");
        host.nvidia("0000:41:00.0", Some(1), "NVIDIA GeForce GTX 1060 6GB");
        host.nvidia("0000:21:00.0", Some(0), "NVIDIA GeForce RTX 3090");
        let cards = host_cards(&host.roots);
        let pcis: Vec<&str> = cards.iter().map(|c| c.pci.as_str()).collect();
        assert_eq!(
            pcis,
            [
                "0000:00:02.0",
                "0000:03:00.0",
                "0000:21:00.0",
                "0000:41:00.0"
            ]
        );
        let amd = &cards[1];
        assert_eq!((amd.vendor, amd.device_id), (AMD, 0x744c));
        assert_eq!(amd.driver, "amdgpu");
        assert_eq!(amd.render, Some(host.roots.dev.join("dri/renderD128")));
        assert_eq!(
            amd.by_path,
            Some(host.roots.dev.join("dri/by-path/pci-0000:03:00.0-render"))
        );
        // The made-up node is no amdgpu device: the ioctl fails, and the
        // answer says it is a guess.
        assert!(
            amd.class
                .how
                .starts_with("guess: the amdgpu info ioctl failed"),
            "{:?}",
            amd.class
        );
        assert_eq!(cards[0].class.kind, Kind::Integrated);
        // CUDA's PCI order, and the minors /proc gives.
        let nvidia: Vec<(u32, u32)> = cards[2..]
            .iter()
            .map(|c| c.nvidia.as_ref().map(|n| (n.index, n.minor)).unwrap())
            .collect();
        assert_eq!(nvidia, [(0, 0), (1, 1)]);
        assert_eq!(cards[3].fallback_name(), "NVIDIA GeForce GTX 1060 6GB");
        assert_eq!(cards[1].fallback_name(), "AMD 1002:744c");
    }

    /// Under systemd's `ProcSubset=pid` /proc/driver/nvidia is hidden: a
    /// card's node is taken from its PCI order.
    #[tokio::test]
    async fn without_proc_an_nvidia_card_s_node_follows_pci_order() {
        let host = FakeHost::new("no-proc").await;
        host.nvidia("0000:41:00.0", None, "");
        host.nvidia("0000:01:00.0", None, "");
        let cards = host_cards(&host.roots);
        let minors: Vec<u32> = cards
            .iter()
            .map(|c| c.nvidia.as_ref().unwrap().minor)
            .collect();
        assert_eq!(minors, [0, 1]);
        assert_eq!(cards[0].fallback_name(), "NVIDIA 10de:1c03");
        assert_eq!(cards[0].class.kind, Kind::Discrete);
    }

    // ---- Discrete or integrated

    #[test]
    fn each_vendor_is_classified_and_says_how() {
        let nvidia = classify(NVIDIA, "nvidia", "0000:01:00.0", None);
        assert_eq!(nvidia.kind, Kind::Discrete);
        assert_eq!(nvidia.how, "NVIDIA: every NVIDIA card is");
        let apu = classify(AMD, "amdgpu", "0000:6d:00.0", Some(Ok(true)));
        assert_eq!(
            (apu.kind, apu.how.as_str()),
            (Kind::Integrated, "amdgpu fusion flag: an APU")
        );
        let card = classify(AMD, "amdgpu", "0000:03:00.0", Some(Ok(false)));
        assert_eq!(card.kind, Kind::Discrete);
        let unknown = classify(
            AMD,
            "amdgpu",
            "0000:03:00.0",
            Some(Err("Permission denied".into())),
        );
        assert!(unknown.how.starts_with("guess:"), "{unknown:?}");
        let igpu = classify(INTEL, "i915", "0000:00:02.0", None);
        assert_eq!(igpu.kind, Kind::Integrated);
        let arc = classify(INTEL, "xe", "0000:03:00.0", None);
        assert_eq!(arc.kind, Kind::Discrete);
        let other = classify(0x1af4, "virtio-pci", "0000:00:01.0", None);
        assert_eq!(other.kind, Kind::Integrated);
        assert!(other.how.starts_with("guess:"), "{other:?}");
    }

    /// `drm_amdgpu_info_device` as the kernel writes it: the device id
    /// first, `ids_flags` at byte 136.
    #[test]
    fn the_amdgpu_fusion_flag_is_read_where_the_kernel_puts_it() {
        let mut bytes = vec![0u8; 256];
        bytes[0..4].copy_from_slice(&0x164e_u32.to_ne_bytes());
        bytes[136..144].copy_from_slice(&0x5_u64.to_ne_bytes());
        assert_eq!(
            AmdgpuDevInfo::decode(&bytes),
            Some(AmdgpuDevInfo {
                device_id: 0x164e,
                fusion: true
            })
        );
        bytes[136..144].copy_from_slice(&0x4_u64.to_ne_bytes());
        assert!(!AmdgpuDevInfo::decode(&bytes).unwrap().fusion);
        assert_eq!(AmdgpuDevInfo::decode(&bytes[..140]), None);
    }

    /// On a host with amdgpu render nodes: the ioctl answers, for the
    /// device sysfs names. Nothing to check on a host without one. On the
    /// dev machine: the RX 7900 XTX (744c) is not an APU, the 7950X3D's
    /// Raphael iGPU (164e) is.
    #[test]
    fn the_amdgpu_ioctl_answers_on_this_host_s_amd_cards() {
        for card in host_cards(&HostRoots::default()) {
            let (Some(render), "amdgpu") = (&card.render, card.driver.as_str()) else {
                continue;
            };
            let info =
                AmdgpuDevInfo::read(render).unwrap_or_else(|e| panic!("{}: {e}", render.display()));
            assert_eq!(
                info.device_id,
                u32::from(card.device_id),
                "{}",
                render.display()
            );
            assert!(
                card.class.how.starts_with("amdgpu fusion flag"),
                "{:?}",
                card.class
            );
            eprintln!(
                "{} {:04x}: fusion {} → {}",
                render.display(),
                card.device_id,
                info.fusion,
                card.class.kind.as_str()
            );
        }
    }

    // ---- Entries take cards

    fn amd(pci: &str, render: &str, kind: Kind) -> HostCard {
        HostCard {
            pci: pci.into(),
            vendor: AMD,
            device_id: 0x744c,
            driver: "amdgpu".into(),
            render: Some(PathBuf::from(render)),
            by_path: None,
            nvidia: None,
            class: Class {
                kind,
                how: "test".into(),
            },
        }
    }

    fn geforce(pci: &str, index: u32) -> HostCard {
        HostCard {
            pci: pci.into(),
            vendor: NVIDIA,
            device_id: 0x1c03,
            driver: "nvidia".into(),
            render: None,
            by_path: None,
            nvidia: Some(NvidiaCard {
                index,
                minor: index,
                model: None,
            }),
            class: classify(NVIDIA, "nvidia", pci, None),
        }
    }

    fn entry(api: Hardware, which: Which) -> GpuEntry {
        GpuEntry {
            api,
            which,
            max_jobs: None,
        }
    }

    fn taken(cards: &[Card]) -> Vec<(usize, &str)> {
        cards
            .iter()
            .map(|c| (c.entry, c.host.pci.as_str()))
            .collect()
    }

    #[test]
    fn a_pick_takes_the_cards_of_its_api_and_kind() {
        let host = [
            amd("0000:03:00.0", "/dev/dri/renderD128", Kind::Discrete),
            amd("0000:6d:00.0", "/dev/dri/renderD129", Kind::Integrated),
            geforce("0000:41:00.0", 0),
        ];
        let (cards, missing) = select(&GpuEntry::defaults(), &host);
        assert_eq!(
            taken(&cards),
            [
                (0, "0000:41:00.0"),
                (1, "0000:03:00.0"),
                (1, "0000:6d:00.0")
            ]
        );
        // No Intel card: Quick Sync takes none, which is no warning.
        assert_eq!(missing.len(), 1);
        assert_eq!((missing[0].entry, missing[0].fixed), (2, false));
        assert_eq!(missing[0].why, "no Intel (Quick Sync) card here");

        let (cards, _) = select(
            &[
                entry(Hardware::Vaapi, Which::Pick(Pick::Igpu)),
                entry(Hardware::Vaapi, Which::Pick(Pick::Dgpu)),
            ],
            &host,
        );
        assert_eq!(taken(&cards), [(0, "0000:6d:00.0"), (1, "0000:03:00.0")]);
    }

    /// The same card under one API is taken once, by the first entry.
    #[test]
    fn a_card_is_taken_once_an_api() {
        let host = [amd("0000:03:00.0", "/dev/dri/renderD128", Kind::Discrete)];
        let (cards, _) = select(
            &[
                entry(Hardware::Vaapi, Which::Pick(Pick::Dgpu)),
                GpuEntry::all(Hardware::Vaapi),
            ],
            &host,
        );
        assert_eq!(taken(&cards), [(0, "0000:03:00.0")]);
    }

    #[tokio::test]
    async fn a_device_names_one_card_by_node_or_cuda_index() {
        let fake = FakeHost::new("device").await;
        fake.render(128, "0000:03:00.0", AMD, 0x744c, "amdgpu");
        fake.render(129, "0000:6d:00.0", AMD, 0x164e, "amdgpu");
        let mut host = host_cards(&fake.roots);
        host.push(geforce("0000:41:00.0", 0));
        let by_path = fake.roots.dev.join("dri/by-path/pci-0000:6d:00.0-render");
        let (cards, missing) = select(
            &[
                entry(
                    Hardware::Vaapi,
                    Which::Device(by_path.display().to_string()),
                ),
                entry(Hardware::Nvenc, Which::Device("0".into())),
                entry(Hardware::Nvenc, Which::Device("1".into())),
                entry(
                    Hardware::Vaapi,
                    Which::Device(fake.roots.dev.join("dri/renderD130").display().to_string()),
                ),
            ],
            &host,
        );
        assert_eq!(taken(&cards), [(0, "0000:6d:00.0"), (1, "0000:41:00.0")]);
        assert_eq!(
            cards[0].vaapi_device(),
            fake.roots.dev.join("dri/renderD129")
        );
        let missed: Vec<(usize, bool)> = missing.iter().map(|m| (m.entry, m.fixed)).collect();
        assert_eq!(missed, [(2, true), (3, true)]);
        assert_eq!(missing[0].why, "no NVIDIA card has CUDA index 1: 1 found");
        assert!(
            missing[1].why.ends_with("is not a GPU's render node here"),
            "{}",
            missing[1].why
        );
    }

    // ---- Ranking

    fn found(card: HostCard, entry: usize, capability: usize) -> Found {
        Found {
            card: Card {
                api: Hardware::Vaapi,
                host: card,
                entry,
            },
            name: String::new(),
            encoders: Encoders::software_baseline(),
            capability,
        }
    }

    fn order(found: &[Found]) -> Vec<&str> {
        found.iter().map(|f| f.card.host.pci.as_str()).collect()
    }

    #[test]
    fn capability_counts_encodes_decodes_and_filters() {
        use crate::encoders::{HardwareDecoder, VideoEncoder};
        use spoolframe_types::VideoCodec;
        let mut e = Encoders::software_baseline();
        for codec in [VideoCodec::H264, VideoCodec::Hevc] {
            e.video.push(VideoEncoder {
                codec,
                name: format!("{}_vaapi", codec.as_str()),
                hardware: Some(Hardware::Vaapi),
            });
        }
        for (codec, bit_depth) in [
            (VideoCodec::Hevc, 8),
            (VideoCodec::Hevc, 10),
            (VideoCodec::Av1, 8),
        ] {
            e.hardware_decoders.push(HardwareDecoder {
                codec,
                hardware: Hardware::Vaapi,
                bit_depth,
            });
        }
        e.gpu_tonemap = true;
        // CUDA's filters are not VA-API's, and libx264 is no GPU's.
        e.cuda_scale = true;
        assert_eq!(capability(&e, Hardware::Vaapi), 2 + 3 + 1);
        assert_eq!(capability(&e, Hardware::Nvenc), 1);
    }

    /// PGPU-04's libplacebo tone map counts as a GPU tone map: a card
    /// whose libplacebo test passed is the more capable; one that tone
    /// maps both ways counts it once.
    #[test]
    fn a_libplacebo_tone_map_counts_once_as_the_card_s_tone_map() {
        use crate::encoders::{Libplacebo, VulkanDevice};
        let without = Encoders::software_baseline();
        let placebo = |hardware| Encoders {
            libplacebo: vec![Libplacebo {
                hardware,
                device: VulkanDevice::Own,
            }],
            ..Encoders::software_baseline()
        };
        assert_eq!(capability(&without, Hardware::Vaapi), 0);
        assert_eq!(capability(&placebo(Hardware::Vaapi), Hardware::Vaapi), 1);
        assert_eq!(capability(&placebo(Hardware::Nvenc), Hardware::Nvenc), 1);
        // Another API's libplacebo is not this card's.
        assert_eq!(capability(&placebo(Hardware::Nvenc), Hardware::Vaapi), 0);
        let both = Encoders {
            gpu_tonemap: true,
            ..placebo(Hardware::Vaapi)
        };
        assert_eq!(capability(&both, Hardware::Vaapi), 1);
    }

    #[test]
    fn the_most_capable_card_comes_first_within_its_entry() {
        let ranked = rank(
            vec![
                found(amd("0000:03:00.0", "/r128", Kind::Discrete), 1, 5),
                found(amd("0000:6d:00.0", "/r129", Kind::Integrated), 1, 9),
                found(amd("0000:c8:00.0", "/r130", Kind::Discrete), 0, 1),
            ],
            &[
                GpuEntry::all(Hardware::Vaapi),
                GpuEntry::all(Hardware::Vaapi),
            ],
        );
        // The entries' order first, however capable.
        assert_eq!(
            order(&ranked),
            ["0000:c8:00.0", "0000:6d:00.0", "0000:03:00.0"]
        );
    }

    #[test]
    fn among_equals_discrete_comes_first_then_the_lower_address() {
        let ranked = rank(
            vec![
                found(amd("0000:6d:00.0", "/r129", Kind::Integrated), 0, 5),
                found(amd("0000:c8:00.0", "/r130", Kind::Discrete), 0, 5),
                found(amd("0000:03:00.0", "/r128", Kind::Discrete), 0, 5),
            ],
            &[GpuEntry::all(Hardware::Vaapi)],
        );
        assert_eq!(
            order(&ranked),
            ["0000:03:00.0", "0000:c8:00.0", "0000:6d:00.0"]
        );
    }

    #[test]
    fn best_keeps_the_one_most_capable_card() {
        let ranked = rank(
            vec![
                found(amd("0000:03:00.0", "/r128", Kind::Discrete), 0, 5),
                found(amd("0000:6d:00.0", "/r129", Kind::Integrated), 0, 6),
            ],
            &[entry(Hardware::Vaapi, Which::Pick(Pick::Best))],
        );
        assert_eq!(order(&ranked), ["0000:6d:00.0"]);
    }

    // ---- Detection, once a card

    /// A stand-in ffmpeg for two VA-API cards: H.264 encodes on each,
    /// HEVC only on renderD128; every run recorded with its GPU.
    #[derive(Default)]
    struct TwoCards {
        runs: Mutex<Vec<(Vec<String>, GpuUse)>>,
    }

    #[async_trait]
    impl Ffmpeg for TwoCards {
        async fn encoders(&self) -> Result<String, String> {
            Ok(" ------\n V....D h264_vaapi VAAPI\n V....D hevc_vaapi VAAPI\n V....D libx264 x264\n".into())
        }

        async fn succeeds(
            &self,
            args: &[String],
            _scratch: &Path,
            gpu: TestGpu,
        ) -> Result<(), String> {
            self.runs.lock().push((args.to_vec(), gpu.gpu));
            let joined = args.join(" ");
            let works = joined.contains("libx264")
                || joined.contains("h264_vaapi")
                || (joined.contains("hevc_vaapi") && joined.contains("renderD128"));
            if works { Ok(()) } else { Err("refused".into()) }
        }

        async fn stderr(&self, args: &[String], _scratch: &Path, _gpu: GpuUse) -> Option<String> {
            let node = args.iter().find(|a| a.starts_with("vaapi=va:"))?;
            Some(format!(
                "[VAAPI @ 0x1] VAAPI driver: Mesa Gallium driver 26.1.8 for card {} (radeonsi).\n",
                node.trim_start_matches("vaapi=va:")
            ))
        }
    }

    /// Ffmpeg confined to one card: the stand-in, shared.
    struct Through<'a>(&'a TwoCards);

    #[async_trait]
    impl Ffmpeg for Through<'_> {
        async fn encoders(&self) -> Result<String, String> {
            self.0.encoders().await
        }
        async fn succeeds(
            &self,
            args: &[String],
            scratch: &Path,
            gpu: TestGpu,
        ) -> Result<(), String> {
            self.0.succeeds(args, scratch, gpu).await
        }
        async fn stderr(&self, args: &[String], scratch: &Path, gpu: GpuUse) -> Option<String> {
            self.0.stderr(args, scratch, gpu).await
        }
    }

    /// A software sample written into the shared scratch runs once for
    /// every card, while its file is there; again once it is gone.
    /// Review 2026-10-11 L62.
    #[tokio::test]
    async fn a_software_sample_is_encoded_once_while_its_file_is_there() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-gpus-sample").await;
        let ffmpeg = TwoCards::default();
        let memo = Memo::default();
        let remembered = Remembered {
            inner: &ffmpeg,
            memo: &memo,
        };
        let file = dir.as_ref().join("sample-h264-8.mkv");
        let args: Vec<String> = ["-c:v", "libx264", file.to_str().unwrap()]
            .map(String::from)
            .to_vec();
        let run = || remembered.succeeds(&args, dir.as_ref(), GpuUse::None.into());
        run().await.unwrap();
        // The stand-in writes nothing: the file it would have.
        std::fs::write(&file, b"sample").unwrap();
        run().await.unwrap();
        run().await.unwrap();
        assert_eq!(ffmpeg.runs.lock().len(), 1, "once, then remembered");
        std::fs::remove_file(&file).unwrap();
        run().await.unwrap();
        assert_eq!(ffmpeg.runs.lock().len(), 2, "the file gone, made again");
    }

    #[tokio::test]
    async fn each_card_is_detected_on_its_own_node_and_software_once() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-gpus-detect").await;
        let ffmpeg: &'static TwoCards = Box::leak(Box::default());
        let host = [
            amd("0000:03:00.0", "/dev/dri/renderD128", Kind::Discrete),
            amd("0000:6d:00.0", "/dev/dri/renderD129", Kind::Integrated),
        ];
        let entries = [GpuEntry::all(Hardware::Vaapi)];
        let (cards, _) = select(&entries, &host);
        let (software, found) = detect_cards(
            &Through(ffmpeg),
            &|_card: &Card| Box::new(Through(ffmpeg)) as Box<dyn Ffmpeg>,
            cards,
            &TranscodeSettings::default(),
            dir.as_ref(),
        )
        .await;
        assert_eq!(
            software
                .video
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["libx264"]
        );
        let hevc: Vec<bool> = found
            .iter()
            .map(|f| f.encoders.video.iter().any(|e| e.name == "hevc_vaapi"))
            .collect();
        assert_eq!(hevc, [true, false]);
        assert_eq!(found[0].name, "card /dev/dri/renderD128");
        assert!(found[0].capability > found[1].capability);
        let runs = ffmpeg.runs.lock();
        // libx264's test ran once for all three detections.
        let x264 = runs
            .iter()
            .filter(|(args, _)| {
                args.iter().any(|a| a == "libx264") && args.ends_with(&["-".to_owned()])
            })
            .count();
        assert_eq!(x264, 1, "{runs:?}");
        // Each card's VA-API tests named its own node only.
        for (args, gpu) in runs
            .iter()
            .filter(|(args, _)| args.iter().any(|a| a.ends_with("_vaapi")))
        {
            assert_eq!(*gpu, GpuUse::Vaapi);
            let nodes: Vec<&String> = args.iter().filter(|a| a.contains("/dev/dri/")).collect();
            assert_eq!(nodes.len(), 1, "{args:?}");
        }
    }

    #[test]
    fn a_card_s_name_is_its_driver_s() {
        assert_eq!(
            vaapi_name(
                "[VAAPI @ 0x5] VAAPI driver: Mesa Gallium driver 26.1.8 for AMD Radeon RX 7900 XTX \
                 (radeonsi, navi31, ACO, DRM 3.64, 6.18.54).\n"
            )
            .as_deref(),
            Some("AMD Radeon RX 7900 XTX")
        );
        assert_eq!(
            vaapi_name("VAAPI driver: Intel iHD driver for Intel(R) Gen Graphics - 24.1.5 ()")
                .as_deref(),
            Some("Intel(R) Gen Graphics - 24.1.5")
        );
        assert_eq!(
            vaapi_name("Initialised VAAPI connection: version 1.24"),
            None
        );
        assert_eq!(
            nvenc_name(
                "[h264_nvenc @ 0x1] [ GPU #0 - < NVIDIA GeForce GTX 1060 6GB > has Compute SM 6.1 ]"
            )
            .as_deref(),
            Some("NVIDIA GeForce GTX 1060 6GB")
        );
    }

    #[test]
    fn picks_parse_as_written() {
        for pick in [Pick::All, Pick::Best, Pick::Dgpu, Pick::Igpu] {
            assert_eq!(Pick::parse(pick.as_str()), Some(pick));
        }
        assert_eq!(Pick::parse("discrete"), None);
    }
}
