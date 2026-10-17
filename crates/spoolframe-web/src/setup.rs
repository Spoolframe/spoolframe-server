//! What the setup wizard and the Settings pages decide (P3-10), apart from
//! the pages: host-compiled, so `cargo test` covers it.

use spoolframe_admin_api::{GpuApi, GpuToneMap, LibraryPathCheck, PathMappingEntry};

/// One step of the wizard. Later phases add and replace steps here (P9
/// adds the native catalog's); a step's id is what the server keeps as
/// done, so an id is never reused for another step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub id: &'static str,
    pub title: &'static str,
}

/// Every step, in order. The first is P3-01's setup page; the last sums
/// up and ends the wizard.
pub const STEPS: &[Step] = &[
    Step {
        id: "admin",
        title: "First admin",
    },
    Step {
        id: "server",
        title: "Server",
    },
    Step {
        id: "plex",
        title: "Plex",
    },
    Step {
        id: "paths",
        title: "Paths",
    },
    Step {
        id: "playback",
        title: "Playback",
    },
    Step {
        id: "access",
        title: "Access",
    },
    Step {
        id: "done",
        title: "Done",
    },
];

/// The step called `id`, and its number from 1.
pub fn step(id: &str) -> Option<(usize, Step)> {
    STEPS
        .iter()
        .position(|s| s.id == id)
        .map(|i| (i + 1, STEPS[i]))
}

/// The step after `id`'s.
pub fn next(id: &str) -> Option<Step> {
    let (n, _) = step(id)?;
    STEPS.get(n).copied()
}

/// The step before `id`'s.
pub fn previous(id: &str) -> Option<Step> {
    let (n, _) = step(id)?;
    n.checked_sub(2).map(|i| STEPS[i])
}

/// Where a wizard left half-way picks up: the first step not confirmed.
/// `done` may name steps this build does not have; they are not counted.
pub fn resume(done: &[String]) -> Step {
    STEPS
        .iter()
        .find(|s| !done.iter().any(|d| d == s.id))
        .copied()
        .unwrap_or(STEPS[STEPS.len() - 1])
}

/// "Steps 1–3 are saved": what is behind a resumed step.
pub fn saved_before(at: Step) -> String {
    match step(at.id).map(|(n, _)| n) {
        Some(1) | None => "Nothing is saved yet.".into(),
        Some(2) => "Step 1 is saved.".into(),
        Some(n) => format!("Steps 1–{} are saved.", n - 1),
    }
}

/// Which GPUs `transcode.gpus` says to use, as the Done card sums it
/// up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hardware {
    /// Not set: every card of every API.
    Auto,
    /// `[]`: no GPU.
    Software,
    /// The entries' APIs in order, each once: `nvenc, vaapi`.
    Ordered(String),
}

impl Hardware {
    /// From `transcode.gpus`: nothing set is every card, an empty list
    /// none, else its APIs in order.
    pub fn from_gpus(value: Option<&serde_json::Value>) -> Self {
        use serde_json::Value;
        match value {
            Some(Value::Array(list)) if list.is_empty() => Hardware::Software,
            Some(Value::Array(list)) => {
                let mut apis: Vec<&str> = Vec::new();
                for api in list.iter().filter_map(|e| e.get("api")?.as_str()) {
                    if !apis.contains(&api) {
                        apis.push(api);
                    }
                }
                Hardware::Ordered(apis.join(", "))
            }
            _ => Hardware::Auto,
        }
    }
}

/// One family of video encoders, as the Playback step lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Family {
    pub name: &'static str,
    /// The codecs it encodes here: "H.264", "HEVC".
    pub codecs: Vec<&'static str>,
}

/// The families the server found working, from ffmpeg's encoder names
/// (`hevc_vaapi`, `libx264`), the hardware ones first. Software is always
/// there.
pub fn families(video_encoders: &[String]) -> Vec<Family> {
    let mut out: Vec<Family> = [
        ("VA-API", "vaapi"),
        ("NVENC", "nvenc"),
        ("Quick Sync", "qsv"),
    ]
    .into_iter()
    .map(|(name, suffix)| Family {
        name,
        codecs: video_encoders
            .iter()
            .filter_map(|e| e.strip_suffix(&format!("_{suffix}")))
            .filter_map(codec_name)
            .collect(),
    })
    .filter(|f| !f.codecs.is_empty())
    .collect();
    out.push(Family {
        name: "Software",
        codecs: video_encoders
            .iter()
            .filter(|e| !e.contains('_') || e.starts_with("lib"))
            .filter_map(|e| codec_name(e.trim_start_matches("lib")))
            .collect(),
    });
    // Two encoders of one codec (libx264 and openh264) name it once.
    for family in &mut out {
        let mut seen = Vec::new();
        family.codecs.retain(|c| {
            let new = !seen.contains(c);
            seen.push(*c);
            new
        });
    }
    out
}

/// What a family's GPU decodes, as the Playback step lists it under the
/// family (PGPU-01): `NVDEC decodes H.264, HEVC, HEVC 10-bit`, from the
/// server's `hevc 10-bit (nvdec)`. `None` where it decodes nothing.
pub fn decoders_line(family: &str, hardware_decoders: &[String]) -> Option<String> {
    let (api, by) = match family {
        "VA-API" => ("vaapi", "VA-API"),
        "NVENC" => ("nvdec", "NVDEC"),
        "Quick Sync" => ("qsv", "Quick Sync"),
        _ => return None,
    };
    let suffix = format!(" ({api})");
    let codecs: Vec<String> = hardware_decoders
        .iter()
        .filter_map(|d| d.strip_suffix(&suffix))
        .filter_map(|d| {
            let (codec, depth) = d.split_once(' ').map_or((d, None), |(c, r)| (c, Some(r)));
            let name = codec_name(codec)?;
            Some(depth.map_or_else(|| name.to_owned(), |depth| format!("{name} {depth}")))
        })
        .collect();
    (!codecs.is_empty()).then(|| format!("{by} decodes {}", codecs.join(", ")))
}

fn codec_name(ffmpeg: &str) -> Option<&'static str> {
    Some(match ffmpeg {
        "h264" | "x264" | "openh264" => "H.264",
        "hevc" | "x265" => "HEVC",
        "av1" | "svtav1" | "aom-av1" | "aom" => "AV1",
        "vp9" | "vpx-vp9" => "VP9",
        _ => return None,
    })
}

/// A tesseract language's name in English, for the few the boards show;
/// its code otherwise.
pub fn language_name(code: &str) -> String {
    match code {
        "eng" => "English",
        "deu" => "German",
        "fra" => "French",
        "spa" => "Spanish",
        "ita" => "Italian",
        "por" => "Portuguese",
        "nld" => "Dutch",
        "rus" => "Russian",
        "ukr" => "Ukrainian",
        "pol" => "Polish",
        "swe" => "Swedish",
        "jpn" => "Japanese",
        "kor" => "Korean",
        "chi_sim" => "Chinese (simplified)",
        "chi_tra" => "Chinese (traditional)",
        other => other,
    }
    .to_owned()
}

/// MiB as the Playback step shows them, in GiB: "20", "1.5".
pub fn gib(mib: i64) -> String {
    if mib % 1024 == 0 {
        (mib / 1024).to_string()
    } else {
        format!("{:.1}", mib as f64 / 1024.0)
    }
}

/// GiB as typed, in MiB; `None` for what is not a positive number.
pub fn mib(gib: &str) -> Option<i64> {
    let n: f64 = gib.trim().parse().ok()?;
    (n > 0.0 && n.is_finite()).then(|| (n * 1024.0).round() as i64)
}

/// A path with no trailing slash, `/` itself kept.
fn trimmed(path: &str) -> &str {
    match path.trim().trim_end_matches('/') {
        "" => "/",
        p => p,
    }
}

/// Where `plex`, in library `library`, is on this server under
/// `mappings`, as the server maps it: a rule for the library before a
/// global one, the longest prefix of whole components first. `None` when
/// no rule matches.
pub fn mapped(mappings: &[PathMappingEntry], library: &str, plex: &str) -> Option<String> {
    let plex = trimmed(plex);
    let matching = |scoped: bool| {
        mappings
            .iter()
            .filter(|m| m.library.as_deref().is_some() == scoped)
            .filter(|m| !scoped || m.library.as_deref() == Some(library))
            .filter_map(|m| {
                let from = trimmed(&m.from);
                let rest = if plex == from {
                    Some("")
                } else if from == "/" {
                    plex.strip_prefix('/')
                } else {
                    plex.strip_prefix(from).and_then(|r| r.strip_prefix('/'))
                }?;
                Some((from.matches('/').count(), m, rest))
            })
            .max_by_key(|(depth, _, _)| *depth)
            .map(|(_, m, rest)| {
                let to = trimmed(&m.to);
                match (to, rest) {
                    (to, "") => to.to_owned(),
                    ("/", rest) => format!("/{rest}"),
                    (to, rest) => format!("{to}/{rest}"),
                }
            })
    };
    matching(true).or_else(|| matching(false))
}

/// One folder of a library on the Paths step: where Plex has it, and
/// where it is here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRow {
    pub library: String,
    pub plex: String,
    pub local: String,
    /// `local` is the server's suggestion, not a saved mapping.
    pub suggested: bool,
}

/// The rows for `checks` under the saved `mappings`: each folder where it
/// maps to, or where the server suggests when its sample fails.
pub fn rows(checks: &[LibraryPathCheck], mappings: &[PathMappingEntry]) -> Vec<PathRow> {
    let mut out = Vec::new();
    for check in checks {
        let failing = check.ok < check.checked;
        for root in &check.roots {
            let saved = mapped(mappings, &check.library, root);
            let suggestion = check
                .suggestion
                .as_ref()
                .filter(|_| failing)
                .and_then(|s| mapped(std::slice::from_ref(s), &check.library, root));
            let (local, suggested) = match (saved, suggestion) {
                (_, Some(suggested)) => (suggested, true),
                (Some(saved), None) => (saved, false),
                (None, None) => (trimmed(root).to_owned(), false),
            };
            out.push(PathRow {
                library: check.library.clone(),
                plex: trimmed(root).to_owned(),
                local,
                suggested,
            });
        }
    }
    out
}

/// The mappings `rows` and the extra ones make: one per folder that is
/// somewhere else here, for its library, then the extra ones as typed.
pub fn mappings(rows: &[PathRow], extra: &[PathMappingEntry]) -> Vec<PathMappingEntry> {
    rows.iter()
        .filter(|r| !r.local.trim().is_empty() && trimmed(&r.local) != trimmed(&r.plex))
        .map(|r| PathMappingEntry {
            library: Some(r.library.clone()),
            from: r.plex.clone(),
            to: trimmed(&r.local).to_owned(),
        })
        .chain(
            extra
                .iter()
                .filter(|m| !m.from.trim().is_empty() && !m.to.trim().is_empty())
                .cloned(),
        )
        .collect()
}

/// Of the saved `mappings`, those no folder of `checks` is under: the
/// rows do not show them, so the Paths step keeps them as extra rows, or
/// Continue would save the rows' mappings without them.
pub fn unattached(
    checks: &[LibraryPathCheck],
    mappings: &[PathMappingEntry],
) -> Vec<PathMappingEntry> {
    mappings
        .iter()
        .filter(|m| {
            !checks.iter().any(|check| {
                check
                    .roots
                    .iter()
                    .any(|root| mapped(std::slice::from_ref(*m), &check.library, root).is_some())
            })
        })
        .cloned()
        .collect()
}

/// A library's sample, as its row says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sample {
    /// No files to look at.
    Empty,
    Ok {
        of: u64,
    },
    Missing {
        missing: u64,
        of: u64,
    },
}

pub fn sample(check: &LibraryPathCheck) -> Sample {
    match (check.checked, check.checked - check.ok.min(check.checked)) {
        (0, _) => Sample::Empty,
        (of, 0) => Sample::Ok { of },
        (of, missing) => Sample::Missing { missing, of },
    }
}

/// The setup summary's warnings (P3-18): "Music: 2 of 5 missing" for each
/// library whose sample has files this server cannot read.
pub fn missing_files(checks: &[LibraryPathCheck]) -> Vec<String> {
    checks
        .iter()
        .filter_map(|c| match sample(c) {
            Sample::Missing { missing, of } => {
                Some(format!("{}: {missing} of {of} missing", c.library))
            }
            _ => None,
        })
        .collect()
}

/// The longest display name the server takes (`setDisplayName`).
const MAX_DISPLAY_NAME: usize = 64;

/// The first admin's display name as typed in step 1 (P3-18), checked as
/// the server checks it before the account is made, so a name it would
/// refuse cannot leave an account made without one. `None` when blank:
/// the username is shown.
pub fn display_name(typed: &str) -> Result<Option<String>, String> {
    let name = typed.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_DISPLAY_NAME || name.chars().any(char::is_control) {
        return Err(format!(
            "A display name is at most {MAX_DISPLAY_NAME} characters, on one line."
        ));
    }
    Ok(Some(name.to_owned()))
}

/// The Done card's Playback line (WebWizardDone): how encoders are
/// chosen, naming the one Automatic picks ("Automatic (VA-API)"), and how
/// many encodes run at once. `video_encoders`: what the server found,
/// `None` until it answers.
pub fn playback_summary(
    hardware: &Hardware,
    video_encoders: Option<&[String]>,
    max_jobs: u64,
) -> String {
    let how = match hardware {
        // Automatic takes the first family that works: the hardware ones
        // come first, software last.
        Hardware::Auto => match video_encoders.map(families) {
            Some(found) => match found.first() {
                Some(f) if f.name != "Software" => format!("Automatic ({})", f.name),
                _ => "Automatic (software)".to_owned(),
            },
            None => "Automatic".to_owned(),
        },
        Hardware::Software => "Software only".to_owned(),
        Hardware::Ordered(list) => format!("In this order: {list}"),
    };
    let jobs = if max_jobs == 1 {
        "1 encode".to_owned()
    } else {
        format!("{max_jobs} encodes")
    };
    format!("{how} · {jobs} at once")
}

/// The Done card's one warning beside Paths: the library with missing
/// files, or how many have some. `None` when every file was found.
pub fn missing_summary(missing: &[String]) -> Option<String> {
    match missing {
        [] => None,
        [one] => Some(one.clone()),
        many => Some(format!("{} libraries have missing files", many.len())),
    }
}

/// What the Playback step says under "Offer picture subtitles as text":
/// with the switch `on`, the languages `found` on the server are what OCR
/// covers. The switch takes effect on a restart, so off wins over the
/// languages the running server found.
pub fn ocr_note(on: bool, found: bool) -> &'static str {
    match (on, found) {
        (false, _) => {
            "Off: every user sees picture subtitles as pictures, drawn by their app or burned in."
        }
        (true, false) => {
            "Tesseract has no languages on this server. Until it has, picture subtitles show as pictures."
        }
        (true, true) => {
            "Found on this server. Picture subtitles in other languages show as pictures. \
             Add Tesseract language data to the server for more."
        }
    }
}

/// What `transcode.gpu_filters` does, under its switch.
pub const GPU_FILTERS_NOTE: &str = "Faster and uses less CPU, but colours or subtitle placement \
     may differ slightly; falls back to the CPU where the GPU can't.";

/// What this server's GPU does of the GPU filters, from detection.
pub fn gpu_filters_support(tonemap: bool, overlay: bool) -> &'static str {
    match (tonemap, overlay) {
        (true, true) => "This GPU tone maps HDR and burns in picture subtitles.",
        (true, false) => "This GPU tone maps HDR. Picture subtitles are burned in on the CPU.",
        (false, true) => "This GPU burns in picture subtitles. HDR is tone mapped on the CPU.",
        (false, false) => {
            "This GPU neither tone maps HDR nor burns in subtitles: the CPU does both."
        }
    }
}

/// `transcode.tone_map`'s choices (PGPU-04): its word, its name, and what
/// it does.
pub const TONE_MAPS: [(&str, &str, &str); 4] = [
    (
        "auto",
        "Automatic",
        "libplacebo on the GPU where it runs, else the CPU. With GPU filters \
         on, the GPU driver's own tone map comes first (Intel).",
    ),
    (
        "cpu",
        "CPU",
        "Always on the CPU: the most CPU, the same picture on any server.",
    ),
    (
        "gpu",
        "The GPU driver's own",
        "tonemap_vaapi, which Intel's VA-API driver has. On the CPU for a GPU without it.",
    ),
    (
        "libplacebo",
        "libplacebo",
        "On the GPU through Vulkan, AMD and NVIDIA alike: fast, and little CPU. \
         On the CPU for a GPU without it.",
    ),
];

/// What this server's GPUs tone map with, from detection: `Found here:
/// VA-API: libplacebo (own) · NVIDIA: libplacebo (shared).`
pub fn tone_maps_line(found: &[GpuToneMap]) -> String {
    let mut per_gpu: Vec<(GpuApi, Vec<&str>)> = Vec::new();
    for tone_map in found {
        match per_gpu.iter_mut().find(|(gpu, _)| *gpu == tone_map.gpu) {
            Some((_, labels)) => labels.push(tone_map.mapper.label()),
            None => per_gpu.push((tone_map.gpu, vec![tone_map.mapper.label()])),
        }
    }
    if per_gpu.is_empty() {
        return "No GPU here tone maps: the CPU does.".to_owned();
    }
    let gpus: Vec<String> = per_gpu
        .into_iter()
        .map(|(gpu, labels)| {
            let name = match gpu {
                GpuApi::Vaapi => "VA-API",
                GpuApi::Nvidia => "NVIDIA",
                GpuApi::Qsv => "Quick Sync",
                GpuApi::Vulkan => "Vulkan",
            };
            format!("{name}: {}", labels.join(", "))
        })
        .collect();
    format!("Found here: {}.", gpus.join(" · "))
}

/// What the experimental section says above its switches.
pub const EXPERIMENTAL_NOTE: &str =
    "These may change or go away in any release, without notice. The server logs the ones on.";

/// One codec of `transcode.video_codecs` (PGPU-06) as Server › Settings
/// › Playback orders it: a row of the list, on (in the setting) or off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecRow {
    /// `av1`, `hevc` or `h264`.
    pub codec: &'static str,
    pub on: bool,
}

/// The codecs a GPU may encode to, what the page calls each, and what it
/// is good for.
pub const VIDEO_CODECS: [(&str, &str, &str); 3] = [
    (
        "av1",
        "AV1",
        "The smallest for what it shows; HDR kept. Only newer devices decode it in hardware.",
    ),
    (
        "hevc",
        "HEVC",
        "About half of H.264's bitrate; HDR kept. Apple devices, TVs and Android decode it.",
    ),
    (
        "h264",
        "H.264",
        "Plays everywhere; no HDR, so HDR is tone mapped.",
    ),
];

/// What the page says of `transcode.video_codecs`.
pub const VIDEO_CODECS_NOTE: &str = "The codecs a GPU encodes to, the first preferred. A \
     conversion takes the first one the device plays (in hardware, where it says) and a GPU \
     here encodes; an HDR film for a screen that shows HDR keeps it in HEVC or AV1 rather \
     than being tone mapped. With none left, it is encoded on the CPU, as H.264. Drag to \
     reorder; untick to leave a codec out.";

/// The rows of the setting's value, its codecs on and in its order, then
/// the others off. Not set: AV1, HEVC, H.264, all on, the server's
/// default.
pub fn codec_rows(value: Option<&serde_json::Value>) -> Vec<CodecRow> {
    let set: Vec<&str> = match value {
        Some(serde_json::Value::Array(list)) => list.iter().filter_map(|v| v.as_str()).collect(),
        _ => VIDEO_CODECS.iter().map(|(codec, ..)| *codec).collect(),
    };
    let mut rows: Vec<CodecRow> = set
        .iter()
        .filter_map(|word| VIDEO_CODECS.iter().find(|(codec, ..)| codec == word))
        .map(|(codec, ..)| CodecRow { codec, on: true })
        .collect();
    for (codec, ..) in VIDEO_CODECS {
        if !rows.iter().any(|row| row.codec == codec) {
            rows.push(CodecRow { codec, on: false });
        }
    }
    rows
}

/// The setting the rows say: the codecs on, in their order.
pub fn codecs_setting(rows: &[CodecRow]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .filter(|row| row.on)
            .map(|row| serde_json::Value::String(row.codec.to_owned()))
            .collect(),
    )
}

/// Per codec of [`VIDEO_CODECS`], the GPU encoders of `encoders` (ffmpeg's
/// names, as the status lists them) that encode it, joined; `None` where
/// none does. Software's (`libx264`) are not a GPU's.
pub fn gpu_encoders_by_codec(encoders: &[String]) -> Vec<(&'static str, Option<String>)> {
    VIDEO_CODECS
        .iter()
        .map(|(codec, ..)| {
            let names: Vec<&str> = encoders
                .iter()
                .map(String::as_str)
                .filter(|name| !name.starts_with("lib"))
                .filter(|name| {
                    name.strip_prefix(codec)
                        .is_some_and(|rest| rest.starts_with('_'))
                })
                .collect();
            (*codec, (!names.is_empty()).then(|| names.join(", ")))
        })
        .collect()
}

/// `list` with the item at `from` moved to `to`, the others keeping their
/// order. Out of range, `list` as it is.
pub fn reordered<T: Clone>(list: &[T], from: usize, to: usize) -> Vec<T> {
    let mut list = list.to_vec();
    if from < list.len() && to < list.len() {
        let item = list.remove(from);
        list.insert(to, item);
    }
    list
}

/// Where a row dragged from `from` lands after the pointer moved `moved`
/// pixels down (up, negative), rows `row` pixels apart, of `count`: the
/// row it is over, rounded to the nearest, within the list.
pub fn drag_target(from: usize, count: usize, moved: f64, row: f64) -> usize {
    if count == 0 || row <= 0.0 {
        return from;
    }
    let steps = (moved / row).round() as i64;
    (from as i64 + steps).clamp(0, count as i64 - 1) as usize
}

/// One entry of `transcode.gpus` (PGPU-05) as Server › Settings ›
/// Playback edits it: as typed, checked at Save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuRow {
    /// `nvenc`, `vaapi` or `qsv`.
    pub api: String,
    /// `all`, `best`, `dgpu`, `igpu`, or `device` for one card.
    pub pick: String,
    /// The card, when `pick` is `device`: a CUDA index or a render node.
    pub device: String,
    /// GPU jobs at once on each of its cards; empty for
    /// `transcode.max_gpu_jobs`.
    pub max_jobs: String,
}

/// The APIs an entry may name, and what the page calls them.
pub const GPU_APIS: [(&str, &str); 3] = [
    ("nvenc", "NVIDIA (NVENC)"),
    ("vaapi", "VA-API (AMD, Intel)"),
    ("qsv", "Quick Sync (Intel)"),
];

/// How an entry may take cards, and what the page calls each.
pub const GPU_PICKS: [(&str, &str); 5] = [
    ("all", "Every card, best first"),
    ("best", "The best card"),
    ("dgpu", "Discrete cards"),
    ("igpu", "Integrated cards"),
    ("device", "This card"),
];

impl GpuRow {
    /// A new row: every VA-API card.
    pub fn new() -> Self {
        Self {
            api: "vaapi".into(),
            pick: "all".into(),
            device: String::new(),
            max_jobs: String::new(),
        }
    }

    /// The rows of the setting's value: a list of `{ api; pick | device;
    /// max_jobs }`. Nothing set is no row (every card of every API).
    pub fn rows(value: Option<&serde_json::Value>) -> Vec<GpuRow> {
        use serde_json::Value;
        let Some(Value::Array(entries)) = value else {
            return Vec::new();
        };
        let text = |entry: &Value, key: &str| match entry.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        entries
            .iter()
            .map(|entry| {
                let device = text(entry, "device");
                GpuRow {
                    api: text(entry, "api"),
                    pick: if device.is_empty() {
                        match text(entry, "pick") {
                            pick if pick.is_empty() => "all".into(),
                            pick => pick,
                        }
                    } else {
                        "device".into()
                    },
                    device,
                    max_jobs: text(entry, "max_jobs"),
                }
            })
            .collect()
    }

    /// The setting for `rows`, or what is wrong with one, numbered as the
    /// page shows them. The server checks it again.
    pub fn setting(rows: &[GpuRow]) -> Result<serde_json::Value, String> {
        let mut entries = Vec::new();
        for (n, row) in rows.iter().enumerate() {
            let n = n + 1;
            let mut entry = serde_json::Map::new();
            entry.insert("api".into(), row.api.clone().into());
            if row.pick == "device" {
                let device = row.device.trim();
                if device.is_empty() {
                    return Err(format!(
                        "GPU {n}: name the card: a CUDA index (0, 1, …) or a render node."
                    ));
                }
                entry.insert("device".into(), device.into());
            } else {
                entry.insert("pick".into(), row.pick.clone().into());
            }
            match row.max_jobs.trim() {
                "" => {}
                typed => match typed.parse::<u32>() {
                    Ok(max) if max > 0 => {
                        entry.insert("max_jobs".into(), max.into());
                    }
                    _ => return Err(format!("GPU {n}: jobs at once is a number, 1 or more.")),
                },
            }
            entries.push(serde_json::Value::Object(entry));
        }
        Ok(serde_json::Value::Array(entries))
    }
}

impl Default for GpuRow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gpu_rows_read_and_write_the_setting() {
        let setting = json!([
            { "api": "nvenc", "pick": "all" },
            { "api": "vaapi", "device": "/dev/dri/renderD128", "max_jobs": 1 },
            { "api": "nvenc", "device": 1 },
            { "api": "qsv" },
        ]);
        let rows = GpuRow::rows(Some(&setting));
        let picks: Vec<(&str, &str, &str, &str)> = rows
            .iter()
            .map(|r| {
                (
                    r.api.as_str(),
                    r.pick.as_str(),
                    r.device.as_str(),
                    r.max_jobs.as_str(),
                )
            })
            .collect();
        assert_eq!(
            picks,
            [
                ("nvenc", "all", "", ""),
                ("vaapi", "device", "/dev/dri/renderD128", "1"),
                ("nvenc", "device", "1", ""),
                ("qsv", "all", "", ""),
            ]
        );
        assert_eq!(
            GpuRow::setting(&rows).unwrap(),
            json!([
                { "api": "nvenc", "pick": "all" },
                { "api": "vaapi", "device": "/dev/dri/renderD128", "max_jobs": 1 },
                { "api": "nvenc", "device": "1" },
                { "api": "qsv", "pick": "all" },
            ])
        );
        assert!(GpuRow::rows(None).is_empty());
    }

    #[test]
    fn a_gpu_row_needs_its_card_and_a_number_of_jobs() {
        let no_card = GpuRow {
            pick: "device".into(),
            ..GpuRow::new()
        };
        assert!(
            GpuRow::setting(&[GpuRow::new(), no_card])
                .unwrap_err()
                .starts_with("GPU 2: ")
        );
        let zero = GpuRow {
            max_jobs: "0".into(),
            ..GpuRow::new()
        };
        assert!(GpuRow::setting(&[zero]).is_err());
    }

    #[test]
    fn the_tone_map_line_names_what_each_gpu_runs() {
        use spoolframe_admin_api::ToneMapper;
        assert_eq!(tone_maps_line(&[]), "No GPU here tone maps: the CPU does.");
        let found = [
            GpuToneMap {
                gpu: GpuApi::Vaapi,
                mapper: ToneMapper::LibplaceboOwn,
            },
            GpuToneMap {
                gpu: GpuApi::Nvidia,
                mapper: ToneMapper::LibplaceboShared,
            },
        ];
        assert_eq!(
            tone_maps_line(&found),
            "Found here: VA-API: libplacebo (own) · NVIDIA: libplacebo (shared)."
        );
    }

    #[test]
    fn every_tone_map_setting_has_a_choice() {
        let words: Vec<&str> = TONE_MAPS.iter().map(|(word, _, _)| *word).collect();
        assert_eq!(words, ["auto", "cpu", "gpu", "libplacebo"]);
    }

    #[test]
    fn the_gpu_filters_note_says_what_this_gpu_does() {
        assert_eq!(
            gpu_filters_support(true, true),
            "This GPU tone maps HDR and burns in picture subtitles."
        );
        assert_eq!(
            gpu_filters_support(true, false),
            "This GPU tone maps HDR. Picture subtitles are burned in on the CPU."
        );
        assert_eq!(
            gpu_filters_support(false, true),
            "This GPU burns in picture subtitles. HDR is tone mapped on the CPU."
        );
        assert_eq!(
            gpu_filters_support(false, false),
            "This GPU neither tone maps HDR nor burns in subtitles: the CPU does both."
        );
    }

    #[test]
    fn ocr_with_languages_names_what_the_languages_cover() {
        assert_eq!(
            ocr_note(true, true),
            "Found on this server. Picture subtitles in other languages show as pictures. \
             Add Tesseract language data to the server for more."
        );
    }

    #[test]
    fn ocr_on_without_languages_says_pictures_are_shown_until_there_are_some() {
        assert_eq!(
            ocr_note(true, false),
            "Tesseract has no languages on this server. Until it has, picture subtitles show as pictures."
        );
    }

    #[test]
    fn ocr_off_says_every_user_gets_pictures_whatever_languages_were_found() {
        for found in [true, false] {
            assert_eq!(
                ocr_note(false, found),
                "Off: every user sees picture subtitles as pictures, drawn by their app or burned in."
            );
        }
    }

    #[test]
    fn automatic_names_the_hardware_it_picks() {
        let found = ["h264_vaapi".to_owned(), "libx264".to_owned()];
        assert_eq!(
            playback_summary(&Hardware::Auto, Some(&found), 2),
            "Automatic (VA-API) · 2 encodes at once"
        );
    }

    #[test]
    fn automatic_without_hardware_says_software() {
        let found = ["libx264".to_owned()];
        assert_eq!(
            playback_summary(&Hardware::Auto, Some(&found), 1),
            "Automatic (software) · 1 encode at once"
        );
    }

    #[test]
    fn automatic_before_the_server_answers_names_nothing() {
        assert_eq!(
            playback_summary(&Hardware::Auto, None, 2),
            "Automatic · 2 encodes at once"
        );
    }

    #[test]
    fn one_library_missing_files_is_named() {
        assert_eq!(
            missing_summary(&["Music: 2 of 5 missing".to_owned()]).as_deref(),
            Some("Music: 2 of 5 missing")
        );
    }

    #[test]
    fn several_libraries_missing_files_are_one_warning() {
        assert_eq!(
            missing_summary(&[
                "Music: 2 of 5 missing".to_owned(),
                "Kids: 1 of 9 missing".to_owned()
            ])
            .as_deref(),
            Some("2 libraries have missing files")
        );
    }

    #[test]
    fn nothing_missing_warns_of_nothing() {
        assert_eq!(missing_summary(&[]), None);
    }

    fn rule(library: Option<&str>, from: &str, to: &str) -> PathMappingEntry {
        PathMappingEntry {
            library: library.map(str::to_owned),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn a_wizard_resumes_at_the_first_step_not_confirmed() {
        let done = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(resume(&done(&[])).id, "admin");
        assert_eq!(resume(&done(&["admin", "server", "plex"])).id, "paths");
        // Out of order, and a step from another build, count as they are.
        assert_eq!(resume(&done(&["server", "admin", "import"])).id, "plex");
        let all: Vec<String> = STEPS.iter().map(|s| s.id.to_owned()).collect();
        assert_eq!(resume(&all).id, "done");
        assert_eq!(
            saved_before(resume(&done(&["admin", "server", "plex"]))),
            "Steps 1–3 are saved."
        );
    }

    #[test]
    fn steps_are_numbered_and_walked_in_order() {
        assert_eq!(step("plex").map(|(n, s)| (n, s.title)), Some((3, "Plex")));
        assert_eq!(next("plex").unwrap().id, "paths");
        assert_eq!(previous("plex").unwrap().id, "server");
        assert_eq!(previous("admin"), None);
        assert_eq!(next("done"), None);
        assert_eq!(STEPS.len(), 7);
    }

    /// The Done card reads `transcode.gpus` as the server does: unset
    /// every card, `[]` none, else its APIs in order.
    #[test]
    fn hardware_is_read_from_the_gpus() {
        assert_eq!(Hardware::from_gpus(None), Hardware::Auto);
        assert_eq!(Hardware::from_gpus(Some(&json!([]))), Hardware::Software);
        assert_eq!(
            Hardware::from_gpus(Some(&json!([
                { "api": "nvenc", "pick": "all" },
                { "api": "vaapi", "device": "/dev/dri/renderD128" },
                { "api": "nvenc", "device": 1 },
            ]))),
            Hardware::Ordered("nvenc, vaapi".into())
        );
    }

    #[test]
    fn encoders_are_grouped_by_family_with_software_last() {
        let found = families(&[
            "hevc_vaapi".into(),
            "h264_vaapi".into(),
            "libx264".into(),
            "libx265".into(),
        ]);
        assert_eq!(
            found,
            [
                Family {
                    name: "VA-API",
                    codecs: vec!["HEVC", "H.264"]
                },
                Family {
                    name: "Software",
                    codecs: vec!["H.264", "HEVC"]
                },
            ]
        );
    }

    #[test]
    fn what_a_family_decodes_is_named_under_it() {
        let found: Vec<String> = [
            "h264 (vaapi)",
            "h264 (nvdec)",
            "hevc (nvdec)",
            "hevc 10-bit (nvdec)",
            "vp9 (nvdec)",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            decoders_line("NVENC", &found).as_deref(),
            Some("NVDEC decodes H.264, HEVC, HEVC 10-bit, VP9")
        );
        assert_eq!(
            decoders_line("VA-API", &found).as_deref(),
            Some("VA-API decodes H.264")
        );
        assert_eq!(decoders_line("Quick Sync", &found), None);
        assert_eq!(decoders_line("Software", &found), None);
    }

    #[test]
    fn scratch_space_is_shown_and_typed_in_gib() {
        assert_eq!(gib(20480), "20");
        assert_eq!(gib(1536), "1.5");
        assert_eq!(mib("20"), Some(20480));
        assert_eq!(mib(" 1.5 "), Some(1536));
        assert_eq!(mib("0"), None);
        assert_eq!(mib("lots"), None);
    }

    #[test]
    fn a_folder_maps_as_the_server_maps_it() {
        let rules = [
            rule(None, "/data", "/mnt/nas"),
            rule(Some("Music"), "/data/music", "/srv/music/"),
        ];
        assert_eq!(
            mapped(&rules, "Movies", "/data/movies").as_deref(),
            Some("/mnt/nas/movies")
        );
        assert_eq!(
            mapped(&rules, "Music", "/data/music").as_deref(),
            Some("/srv/music")
        );
        // Whole components only.
        assert_eq!(mapped(&rules, "Movies", "/database"), None);
        assert_eq!(
            mapped(&[rule(None, "/", "/x")], "Movies", "/a").as_deref(),
            Some("/x/a")
        );
    }

    fn check(library: &str, root: &str, ok: u64, checked: u64) -> LibraryPathCheck {
        LibraryPathCheck {
            library: library.into(),
            roots: vec![root.into()],
            checked,
            ok,
            failures: Vec::new(),
            suggestion: None,
        }
    }

    #[test]
    fn a_failing_folder_takes_the_suggestion_and_a_passing_one_keeps_its_mapping() {
        let failing = LibraryPathCheck {
            suggestion: Some(rule(Some("Movies"), "/data", "/mnt/nas")),
            ..check("Movies", "/data/movies", 0, 5)
        };
        let passing = check("Kids", "/mnt/kids", 5, 5);
        let saved = [rule(Some("Kids"), "/mnt/kids", "/srv/kids")];
        let rows = rows(&[failing, passing], &saved);
        assert_eq!(
            rows,
            [
                PathRow {
                    library: "Movies".into(),
                    plex: "/data/movies".into(),
                    local: "/mnt/nas/movies".into(),
                    suggested: true,
                },
                PathRow {
                    library: "Kids".into(),
                    plex: "/mnt/kids".into(),
                    local: "/srv/kids".into(),
                    suggested: false,
                },
            ]
        );
        assert_eq!(
            mappings(&rows, &[rule(None, "/x", "")]),
            [
                rule(Some("Movies"), "/data/movies", "/mnt/nas/movies"),
                rule(Some("Kids"), "/mnt/kids", "/srv/kids"),
            ]
        );
    }

    /// Saved mappings that no library folder shows are kept on Continue
    /// (P3-18): a rule for a library not served yet, or for a prefix no
    /// folder is under.
    #[test]
    fn saved_mappings_no_folder_shows_are_kept_as_extra_ones() {
        let checks = [
            check("Movies", "/data/movies", 5, 5),
            check("Kids", "/mnt/kids", 5, 5),
        ];
        let saved = [
            rule(Some("Kids"), "/mnt/kids", "/srv/kids"),
            rule(None, "/data", "/mnt/nas"),
            rule(None, "/music", "/srv/music"),
            rule(Some("Documentaries"), "/data/docs", "/srv/docs"),
            rule(Some("Movies"), "/elsewhere", "/srv/elsewhere"),
        ];
        let extra = unattached(&checks, &saved);
        assert_eq!(
            extra,
            [
                rule(None, "/music", "/srv/music"),
                rule(Some("Documentaries"), "/data/docs", "/srv/docs"),
                rule(Some("Movies"), "/elsewhere", "/srv/elsewhere"),
            ]
        );
        // Continue saves the rows' mappings and those.
        assert_eq!(
            mappings(&rows(&checks, &saved), &extra),
            [
                rule(Some("Movies"), "/data/movies", "/mnt/nas/movies"),
                rule(Some("Kids"), "/mnt/kids", "/srv/kids"),
                rule(None, "/music", "/srv/music"),
                rule(Some("Documentaries"), "/data/docs", "/srv/docs"),
                rule(Some("Movies"), "/elsewhere", "/srv/elsewhere"),
            ]
        );
        // With no folders known (the check failed), every one is kept.
        assert_eq!(unattached(&[], &saved), saved);
    }

    #[test]
    fn a_folder_at_the_same_path_here_needs_no_mapping() {
        let rows = rows(&[check("Movies", "/data/movies/", 5, 5)], &[]);
        assert_eq!(rows[0].local, "/data/movies");
        assert_eq!(mappings(&rows, &[]), []);
    }

    #[test]
    fn a_sample_says_how_many_of_it_are_missing() {
        assert_eq!(sample(&check("M", "/m", 5, 5)), Sample::Ok { of: 5 });
        assert_eq!(
            sample(&check("M", "/m", 3, 5)),
            Sample::Missing { missing: 2, of: 5 }
        );
        assert_eq!(sample(&check("M", "/m", 0, 0)), Sample::Empty);
    }

    #[test]
    fn the_summary_names_each_library_with_files_the_server_cannot_read() {
        let checks = [
            check("Movies", "/m", 5, 5),
            check("Music", "/music", 3, 5),
            check("Empty", "/e", 0, 0),
            check("Kids", "/k", 0, 5),
        ];
        assert_eq!(
            missing_files(&checks),
            ["Music: 2 of 5 missing", "Kids: 5 of 5 missing"]
        );
        assert!(missing_files(&checks[..1]).is_empty());
    }

    #[test]
    fn the_first_admin_s_display_name_is_checked_as_the_server_does() {
        assert_eq!(display_name("  Andrey "), Ok(Some("Andrey".into())));
        assert_eq!(display_name("   "), Ok(None));
        assert_eq!(display_name(&"é".repeat(64)), Ok(Some("é".repeat(64))));
        assert!(display_name(&"a".repeat(65)).is_err());
        assert!(display_name("two\nlines").is_err());
    }

    /// PGPU-06: the codec list's rows are the setting's codecs in order,
    /// on, then the rest off; not set is the server's default; saving
    /// writes the ones on in order.
    #[test]
    fn the_codec_rows_follow_the_setting() {
        let order = |rows: &[CodecRow]| -> Vec<(&str, bool)> {
            rows.iter().map(|r| (r.codec, r.on)).collect()
        };
        assert_eq!(
            order(&codec_rows(None)),
            [("av1", true), ("hevc", true), ("h264", true)]
        );
        let set = serde_json::json!(["h264", "hevc"]);
        let rows = codec_rows(Some(&set));
        assert_eq!(
            order(&rows),
            [("h264", true), ("hevc", true), ("av1", false)]
        );
        assert_eq!(codecs_setting(&rows), set);
        assert_eq!(
            order(&codec_rows(Some(&serde_json::json!([])))),
            [("av1", false), ("hevc", false), ("h264", false)]
        );
        assert_eq!(
            codecs_setting(&codec_rows(Some(&serde_json::json!([])))),
            serde_json::json!([])
        );
    }

    #[test]
    fn each_codec_names_the_gpu_encoders_here() {
        let found = gpu_encoders_by_codec(&[
            "h264_vaapi".into(),
            "libx264".into(),
            "hevc_vaapi".into(),
            "hevc_nvenc".into(),
        ]);
        assert_eq!(
            found,
            [
                ("av1", None),
                ("hevc", Some("hevc_vaapi, hevc_nvenc".into())),
                ("h264", Some("h264_vaapi".into())),
            ]
        );
    }

    /// Dragging, or the arrows, move one row and keep the rest in order.
    #[test]
    fn a_row_moves_and_the_rest_keep_their_order() {
        let list = ["av1", "hevc", "h264"];
        assert_eq!(reordered(&list, 0, 2), ["hevc", "h264", "av1"]);
        assert_eq!(reordered(&list, 2, 0), ["h264", "av1", "hevc"]);
        assert_eq!(reordered(&list, 1, 1), list);
        assert_eq!(reordered(&list, 3, 0), list, "out of range");
        // 44 px rows: half a row rounds to the next, and the ends hold.
        assert_eq!(drag_target(0, 3, 30.0, 44.0), 1);
        assert_eq!(drag_target(0, 3, 20.0, 44.0), 0);
        assert_eq!(drag_target(2, 3, -90.0, 44.0), 0);
        assert_eq!(drag_target(1, 3, 500.0, 44.0), 2);
        assert_eq!(drag_target(1, 3, -500.0, 44.0), 0);
    }
}
