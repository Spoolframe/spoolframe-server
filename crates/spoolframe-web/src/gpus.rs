//! What the Server page's GPUs say (PGPU-05), apart from the page: each
//! card's lines, why another does nothing, and where typical conversions
//! run, each as the order the server's pick tries the cards in.

use spoolframe_admin_api::{
    CodecPick, FilterPlace, GpuApi, GpuCard, GpuRoute, OtherGpu, OtherGpuWhy, RouteJob, RouteShare,
    RouteStep, VideoDecoder,
};

/// What the server says when GPU filters being off is why a filter runs
/// on the CPU ([`RouteStep::cpu_why`]): the table then points at the
/// setting.
pub const FILTERS_OFF: &str = "GPU filters are off (transcode.gpu_filters)";

/// `VA-API`, `NVIDIA`: an API as the page names it.
pub fn api_label(api: GpuApi) -> &'static str {
    match api {
        GpuApi::Vaapi => "VA-API",
        GpuApi::Nvidia => "NVIDIA",
        GpuApi::Qsv => "Quick Sync",
        GpuApi::Vulkan => "Vulkan",
    }
}

/// A card's lines under its name: what it is, which entry took it and
/// its jobs; then what it does, each `(label, value)`, leaving out what
/// it has none of; what it lacks last.
pub fn card_lines(card: &GpuCard) -> (String, Vec<(&'static str, String)>) {
    let limit = if card.own_limit {
        "its entry's max_jobs"
    } else {
        "transcode.max_gpu_jobs"
    };
    let about = format!(
        "{} · {} · {} · {} of {} jobs ({limit})",
        api_label(card.api),
        card.kind.label(),
        card.entry,
        card.jobs,
        card.max_jobs,
    );
    let mut lines = Vec::new();
    let mut add = |label, values: Vec<String>| {
        if !values.is_empty() {
            lines.push((label, values.join(", ")));
        }
    };
    add("Encodes", card.encodes.clone());
    add("Decodes", card.decodes.clone());
    add("Filters", card.filters.clone());
    add(
        "Tone maps",
        card.tone_maps
            .iter()
            .map(|m| m.label().to_owned())
            .collect(),
    );
    add("Lacks", card.lacks.clone());
    (about, lines)
}

/// Why `other` does none of the work: "No transcode.gpus entry takes
/// it", "transcode.gpus entry 1 (nvenc pick=all): no NVIDIA test passed
/// (h264_nvenc first)".
pub fn other_why(other: &OtherGpu) -> String {
    match &other.why {
        OtherGpuWhy::NoEntry => "No transcode.gpus entry takes it".to_owned(),
        OtherGpuWhy::NothingWorks { api, entry, test } => match test {
            Some(test) => format!("{entry}: no {} test passed ({test} first)", api_label(*api)),
            None => format!("{entry}: no {} test passed", api_label(*api)),
        },
    }
}

/// The table's columns: the codecs converted to.
pub const TARGETS: [&str; 3] = ["h264", "hevc", "av1"];

/// The routes' rows, in the server's order: each job of a source, and its
/// routes under [`TARGETS`], `None` where it has none (H.264 keeps no
/// HDR, PGPU-06).
pub fn route_rows(routes: &[GpuRoute]) -> Vec<(String, Vec<Option<&GpuRoute>>)> {
    let mut rows: Vec<(String, Vec<Option<&GpuRoute>>)> = Vec::new();
    for route in routes {
        let label = route_label(route);
        let column = TARGETS.iter().position(|t| *t == route.target);
        let row = match rows.iter().position(|(l, _)| *l == label) {
            Some(at) => at,
            None => {
                rows.push((label, vec![None; TARGETS.len()]));
                rows.len() - 1
            }
        };
        if let Some(column) = column {
            rows[row].1[column] = Some(route);
        }
    }
    rows
}

/// A row's source as the table heads it: "HEVC", "4K HDR10 HEVC → SDR",
/// "4K HDR10 HEVC, HDR kept", "H.264 + picture subtitles".
fn route_label(route: &GpuRoute) -> String {
    let codec = codec_label(&route.source);
    match route.job {
        RouteJob::Convert => codec.to_owned(),
        RouteJob::ToneMap => format!("4K HDR10 {codec} → SDR"),
        RouteJob::KeepHdr => format!("4K HDR10 {codec}, HDR kept"),
        RouteJob::BurnIn => format!("{codec} + picture subtitles"),
    }
}

/// What the server converts a 4K HDR10 film to for each kind of device
/// (PGPU-06): "An HDR screen that decodes AV1: AV1, HDR kept · AMD Radeon
/// RX 7900 XTX", the card being the first its route takes.
pub fn pick_lines(picks: &[CodecPick], routes: &[GpuRoute], gpus: &[GpuCard]) -> Vec<String> {
    picks
        .iter()
        .map(|pick| {
            let Some(codec) = &pick.codec else {
                return format!(
                    "{}: nothing here encodes a codec it plays",
                    pick.device.label()
                );
            };
            let job = if pick.hdr_kept {
                RouteJob::KeepHdr
            } else {
                RouteJob::ToneMap
            };
            let card = routes
                .iter()
                .find(|r| r.job == job && r.target == *codec)
                .and_then(|r| r.steps.first())
                .filter(|step| step.share != RouteShare::None)
                .map(|step| step_place(step, gpus));
            let what = if pick.hdr_kept {
                "HDR kept"
            } else {
                "tone mapped"
            };
            match card {
                Some(card) => format!(
                    "{}: {}, {what} · {card}",
                    pick.device.label(),
                    codec_label(codec)
                ),
                None => format!("{}: {}, {what}", pick.device.label(), codec_label(codec)),
            }
        })
        .collect()
}

/// `H.264`, `HEVC`, `AV1`: a protocol codec name as the table heads it.
pub fn codec_label(codec: &str) -> &str {
    match codec {
        "h264" => "H.264",
        "hevc" => "HEVC",
        "av1" => "AV1",
        other => other,
    }
}

/// A route's cell, as the table shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Where it runs first: a card's name, "CPU", or "—" when nothing
    /// here encodes the target.
    pub first: String,
    /// How much of it runs there, and its encoder.
    pub how: String,
    /// Where it goes when the ones before are busy: "Radeon Graphics
    /// (encode only)", then "CPU" where a full GPU spills to a software
    /// encode (H.264), or "next codec" where nothing else encodes it and
    /// a playback takes the next of `transcode.video_codecs` (PGPU-06).
    pub then: Vec<String>,
    /// Why the tone map or burn-in runs on the CPU at the first choice:
    /// "tone map on CPU: GPU filters are off (transcode.gpu_filters)".
    pub why: Option<String>,
}

/// `route` as its cell says it, naming each card by its name in `gpus`.
pub fn route_cell(route: &GpuRoute, gpus: &[GpuCard]) -> Cell {
    let Some(first) = route.steps.first() else {
        return not_offered();
    };
    if first.share == RouteShare::None {
        return not_offered();
    }
    let mut then: Vec<String> = route.steps[1..]
        .iter()
        .map(|step| {
            // A software encode after the cards: the spill, whichever
            // card decodes for it.
            if matches!(step.share, RouteShare::DecodeOnly | RouteShare::Cpu) {
                return "CPU".to_owned();
            }
            let place = step_place(step, gpus);
            if step.share == first.share {
                place
            } else {
                format!("{place} ({})", step.share.label())
            }
        })
        .collect();
    if route.busy_after {
        then.push("next codec".to_owned());
    }
    Cell {
        first: step_place(first, gpus),
        how: step_how(route.job, first),
        then,
        why: cpu_why(route.job, first),
    }
}

fn not_offered() -> Cell {
    Cell {
        first: "—".to_owned(),
        how: RouteShare::None.label().to_owned(),
        then: Vec::new(),
        why: None,
    }
}

fn step_place(step: &RouteStep, gpus: &[GpuCard]) -> String {
    match &step.gpu {
        Some(key) => card_name(key, gpus),
        None => "CPU".to_owned(),
    }
}

/// "all on GPU · h264_vaapi · tone map libplacebo (own)": how much of
/// the job a step does, its encoder, and for the tone map and burn-in
/// rows where that filter runs.
fn step_how(job: RouteJob, step: &RouteStep) -> String {
    let mut how = match (step.share, step.encoder.as_ref()) {
        (RouteShare::Cpu, Some(encoder)) => encoder.name.clone(),
        (share, Some(encoder)) => format!("{} · {}", share.label(), encoder.name),
        (share, None) => share.label().to_owned(),
    };
    if step.share == RouteShare::EncodeOnly && step.decoder == VideoDecoder::Software {
        how.push_str(" · CPU decodes");
    }
    match job {
        RouteJob::ToneMap => match (step.tone_map, step.tone_mapper) {
            (FilterPlace::Cpu, _) => how.push_str(" · tone map on CPU"),
            (_, Some(mapper)) => how.push_str(&format!(" · tone map {}", mapper.label())),
            (_, None) => {}
        },
        RouteJob::BurnIn => match step.burn_in {
            FilterPlace::Gpu => how.push_str(" · burn-in on GPU"),
            FilterPlace::Cpu => how.push_str(" · burn-in on CPU"),
            FilterPlace::None => {}
        },
        RouteJob::KeepHdr => how.push_str(" · HDR kept"),
        RouteJob::Convert => {}
    }
    how
}

/// Why the row's own filter runs on the CPU at `step`, when it does.
fn cpu_why(job: RouteJob, step: &RouteStep) -> Option<String> {
    let filter = match job {
        RouteJob::ToneMap if step.tone_map == FilterPlace::Cpu => "tone map",
        RouteJob::BurnIn if step.burn_in == FilterPlace::Cpu => "burn-in",
        _ => return None,
    };
    if step.cpu_why.is_empty() {
        return None;
    }
    Some(format!("{filter} on CPU: {}", step.cpu_why.join("; ")))
}

/// Whether any route's first choice runs a filter on the CPU because
/// GPU filters are off: the page then says where to turn them on.
pub fn filters_off_matter(routes: &[GpuRoute]) -> bool {
    routes.iter().any(|route| {
        route.steps.first().is_some_and(|step| {
            cpu_why(route.job, step).is_some() && step.cpu_why.iter().any(|w| w == FILTERS_OFF)
        })
    })
}

/// The card `key` names, by its name; with two of one name, its place
/// beside.
fn card_name(key: &str, gpus: &[GpuCard]) -> String {
    let Some(card) = gpus.iter().find(|c| c.key == key) else {
        return key.to_owned();
    };
    if gpus.iter().filter(|c| c.name == card.name).count() > 1 {
        format!("{} ({})", card.name, card.place)
    } else {
        card.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_admin_api::{GpuKind, ToneMapper, VideoEncoderUsed};

    const DGPU: &str = "vaapi:0000:03:00.0";
    const IGPU: &str = "vaapi:0000:6d:00.0";

    fn card(key: &str, name: &str) -> GpuCard {
        GpuCard {
            key: key.into(),
            name: name.into(),
            api: GpuApi::Vaapi,
            place: format!("/dev/dri/by-path/pci-{key}-render"),
            kind: GpuKind::Discrete,
            classified: "amdgpu fusion flag: not an APU".into(),
            entry: "transcode.gpus entry 1 (vaapi pick=all)".into(),
            jobs: 1,
            max_jobs: 2,
            own_limit: false,
            encodes: vec!["h264_vaapi".into(), "hevc_vaapi".into()],
            decodes: vec!["hevc 10-bit (vaapi)".into()],
            filters: Vec::new(),
            tone_maps: vec![ToneMapper::LibplaceboOwn],
            lacks: vec!["VA-API tone map (tonemap_vaapi)".into()],
        }
    }

    fn gpus() -> [GpuCard; 2] {
        [
            card(DGPU, "AMD Radeon RX 7900 XTX"),
            card(IGPU, "AMD Radeon Graphics"),
        ]
    }

    fn step(gpu: Option<&str>, share: RouteShare, target: &str) -> RouteStep {
        RouteStep {
            gpu: gpu.map(Into::into),
            share,
            decoder: VideoDecoder::Gpu { api: GpuApi::Vaapi },
            encoder: Some(VideoEncoderUsed {
                codec: target.into(),
                name: format!("{target}_vaapi"),
                gpu: Some(GpuApi::Vaapi),
            }),
            tone_map: FilterPlace::None,
            tone_mapper: None,
            burn_in: FilterPlace::None,
            cpu_why: Vec::new(),
        }
    }

    fn route(
        job: RouteJob,
        source: &str,
        target: &str,
        steps: Vec<RouteStep>,
        busy_after: bool,
    ) -> GpuRoute {
        GpuRoute {
            job,
            source: source.into(),
            target: target.into(),
            steps,
            busy_after,
        }
    }

    #[test]
    fn a_card_says_what_it_is_and_does_and_leaves_out_what_it_has_none_of() {
        let (about, lines) = card_lines(&card(DGPU, "AMD Radeon RX 7900 XTX"));
        assert_eq!(
            about,
            "VA-API · discrete · transcode.gpus entry 1 (vaapi pick=all) · 1 of 2 jobs \
             (transcode.max_gpu_jobs)"
        );
        let labels: Vec<_> = lines.iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, ["Encodes", "Decodes", "Tone maps", "Lacks"]);
        assert_eq!(lines[0].1, "h264_vaapi, hevc_vaapi");
        assert_eq!(lines[2].1, "libplacebo (own)");
    }

    /// The order the pick tries: the dGPU first, the iGPU next, then the
    /// CPU the full cards spill to (or busy where it cannot encode); AV1
    /// out, which the iGPU cannot encode, has only the dGPU.
    #[test]
    fn a_cell_says_the_first_choice_then_the_next() {
        let gpus = gpus();
        let mut spill = step(Some(DGPU), RouteShare::DecodeOnly, "h264");
        spill.encoder.as_mut().unwrap().name = "libx264".into();
        let h264 = route(
            RouteJob::Convert,
            "hevc",
            "h264",
            vec![
                step(Some(DGPU), RouteShare::Whole, "h264"),
                step(Some(IGPU), RouteShare::Whole, "h264"),
                spill,
            ],
            false,
        );
        assert_eq!(
            route_cell(&h264, &gpus),
            Cell {
                first: "AMD Radeon RX 7900 XTX".into(),
                how: "all on GPU · h264_vaapi".into(),
                then: vec!["AMD Radeon Graphics".into(), "CPU".into()],
                why: None,
            }
        );
        // A next card doing less of the job says how much.
        let av1_in = route(
            RouteJob::Convert,
            "av1",
            "h264",
            vec![
                step(Some(DGPU), RouteShare::Whole, "h264"),
                step(Some(IGPU), RouteShare::EncodeOnly, "h264"),
            ],
            true,
        );
        assert_eq!(
            route_cell(&av1_in, &gpus).then,
            ["AMD Radeon Graphics (encode only)", "next codec"]
        );
        let av1_out = route(
            RouteJob::Convert,
            "hevc",
            "av1",
            vec![step(Some(DGPU), RouteShare::Whole, "av1")],
            true,
        );
        assert_eq!(route_cell(&av1_out, &gpus).then, ["next codec"]);
    }

    #[test]
    fn a_software_route_and_one_not_offered() {
        let gpus = gpus();
        let mut cpu = step(None, RouteShare::Cpu, "av1");
        cpu.encoder.as_mut().unwrap().name = "libsvtav1".into();
        let cell = route_cell(
            &route(RouteJob::Convert, "av1", "av1", vec![cpu], false),
            &gpus,
        );
        assert_eq!(
            (cell.first.as_str(), cell.how.as_str()),
            ("CPU", "libsvtav1")
        );
        assert!(cell.then.is_empty());
        let none = route(
            RouteJob::Convert,
            "av1",
            "hevc",
            vec![RouteStep {
                encoder: None,
                ..step(None, RouteShare::None, "hevc")
            }],
            false,
        );
        assert_eq!(route_cell(&none, &gpus).first, "—");
    }

    /// A tone map on the CPU says why, and the page points at the
    /// setting when it is GPU filters being off.
    #[test]
    fn a_tone_map_on_the_cpu_says_why() {
        let gpus = gpus();
        let mut first = step(Some(DGPU), RouteShare::DecodeAndEncode, "h264");
        first.tone_map = FilterPlace::Cpu;
        first.tone_mapper = Some(ToneMapper::Cpu);
        first.cpu_why = vec![FILTERS_OFF.into()];
        let routes = [route(RouteJob::ToneMap, "hevc", "h264", vec![first], true)];
        let cell = route_cell(&routes[0], &gpus);
        assert_eq!(cell.how, "decode + encode · h264_vaapi · tone map on CPU");
        assert_eq!(
            cell.why.as_deref(),
            Some("tone map on CPU: GPU filters are off (transcode.gpu_filters)")
        );
        assert!(filters_off_matter(&routes));
        assert_eq!(route_rows(&routes)[0].0, "4K HDR10 HEVC → SDR");
    }

    /// PGPU-06: the HDR-kept row has HEVC and AV1 only, in their columns;
    /// and each kind of device says what it gets and on which card.
    #[test]
    fn the_hdr_kept_row_and_the_picks() {
        use spoolframe_admin_api::PickDevice;
        let gpus = gpus();
        let routes = [
            route(
                RouteJob::KeepHdr,
                "hevc",
                "hevc",
                vec![step(Some(DGPU), RouteShare::Whole, "hevc")],
                true,
            ),
            route(
                RouteJob::KeepHdr,
                "hevc",
                "av1",
                vec![step(Some(DGPU), RouteShare::Whole, "av1")],
                true,
            ),
            route(
                RouteJob::ToneMap,
                "hevc",
                "av1",
                vec![step(Some(IGPU), RouteShare::DecodeAndEncode, "av1")],
                true,
            ),
        ];
        let rows = route_rows(&routes);
        assert_eq!(rows[0].0, "4K HDR10 HEVC, HDR kept");
        assert_eq!(
            rows[0].1.iter().map(Option::is_some).collect::<Vec<_>>(),
            [false, true, true]
        );
        assert_eq!(
            route_cell(rows[0].1[1].unwrap(), &gpus).how,
            "all on GPU · hevc_vaapi · HDR kept"
        );
        let picks = [
            CodecPick {
                device: PickDevice::HdrAv1,
                codec: Some("av1".into()),
                hdr_kept: true,
            },
            CodecPick {
                device: PickDevice::Sdr,
                codec: Some("av1".into()),
                hdr_kept: false,
            },
            CodecPick {
                device: PickDevice::HdrHevc,
                codec: None,
                hdr_kept: false,
            },
        ];
        assert_eq!(
            pick_lines(&picks, &routes, &gpus),
            [
                "An HDR screen that decodes AV1: AV1, HDR kept · AMD Radeon RX 7900 XTX",
                "An SDR screen: AV1, tone mapped · AMD Radeon Graphics",
                "An HDR screen without AV1 (Apple TV 4K): nothing here encodes a codec it plays",
            ]
        );
    }

    #[test]
    fn two_cards_of_one_name_are_told_apart_by_place() {
        let gpus = [card(DGPU, "AMD Radeon"), card(IGPU, "AMD Radeon")];
        let r = route(
            RouteJob::Convert,
            "h264",
            "h264",
            vec![step(Some(IGPU), RouteShare::Whole, "h264")],
            true,
        );
        assert_eq!(
            route_cell(&r, &gpus).first,
            format!("AMD Radeon (/dev/dri/by-path/pci-{IGPU}-render)")
        );
    }

    #[test]
    fn another_card_says_why_it_does_nothing() {
        let other = |why| OtherGpu {
            name: "NVIDIA GeForce GTX 1060 6GB".into(),
            pci: "0000:01:00.0".into(),
            driver: "nvidia".into(),
            kind: GpuKind::Discrete,
            why,
        };
        assert_eq!(
            other_why(&other(OtherGpuWhy::NoEntry)),
            "No transcode.gpus entry takes it"
        );
        assert_eq!(
            other_why(&other(OtherGpuWhy::NothingWorks {
                api: GpuApi::Nvidia,
                entry: "transcode.gpus entry 1 (nvenc pick=all)".into(),
                test: Some("h264_nvenc".into()),
            })),
            "transcode.gpus entry 1 (nvenc pick=all): no NVIDIA test passed (h264_nvenc first)"
        );
    }
}
