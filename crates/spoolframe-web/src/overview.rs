//! What the admin's Overview says (WebAdminPhone 4, P3-02), apart from the
//! page: a playback's two lines, and the GPUs (PGPU-05): each card's
//! lines, why another does nothing, and where typical conversions run.

use spoolframe_admin_api::{
    FilterPlace, GpuApi, GpuCard, GpuRoute, OtherGpu, OtherGpuWhy, PlanKind, PlaybackRow, RouteJob,
    RouteShare, VideoDecoder,
};

/// A playback as "Playing now" lists it: "Alice · Severance S2 E4", then
/// "Living room Apple TV · direct".
pub fn playing_lines(row: &PlaybackRow) -> (String, String) {
    let who = if row.display_name.is_empty() {
        &row.username
    } else {
        &row.display_name
    };
    let plan = match row.plan {
        PlanKind::Direct => "direct",
        PlanKind::Remux => "remux",
        PlanKind::Transcode => "transcode",
        PlanKind::Unsupported => "cannot play",
    };
    (
        format!("{who} · {}", row.title.replace(" · ", " ")),
        format!("{} · {plan}", row.device),
    )
}

/// `VA-API`, `NVIDIA`: an API as the Overview names it.
pub fn api_label(api: GpuApi) -> &'static str {
    match api {
        GpuApi::Vaapi => "VA-API",
        GpuApi::Nvidia => "NVIDIA",
        GpuApi::Qsv => "Quick Sync",
        GpuApi::Vulkan => "Vulkan",
    }
}

/// A card's lines under its name: what it is, where, which entry took
/// it and its jobs; then what it does, each `(label, value)`, leaving out
/// what it has none of; what it lacks last.
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

/// The routes' rows, in the server's order: each job of a source, and
/// its three targets' routes, H.264, HEVC, AV1 as the server sent them.
pub fn route_rows(routes: &[GpuRoute]) -> Vec<(String, Vec<&GpuRoute>)> {
    let mut rows: Vec<(String, Vec<&GpuRoute>)> = Vec::new();
    for route in routes {
        let label = route_label(route);
        match rows.iter_mut().find(|(l, _)| *l == label) {
            Some((_, cells)) => cells.push(route),
            None => rows.push((label, vec![route])),
        }
    }
    rows
}

/// A row's source as the table heads it: "HEVC", "4K HDR10 HEVC → SDR",
/// "H.264 + picture subtitles".
fn route_label(route: &GpuRoute) -> String {
    let codec = codec_label(&route.source);
    match route.job {
        RouteJob::Convert => codec.to_owned(),
        RouteJob::ToneMap => format!("4K HDR10 {codec} → SDR"),
        RouteJob::BurnIn => format!("{codec} + picture subtitles"),
    }
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

/// A route's cell: the card it runs on (by its name in `gpus`, "CPU" for
/// none, "—" when not offered), and under it how much it does there,
/// with where the tone map or burn-in runs for those rows.
pub fn route_cell(route: &GpuRoute, gpus: &[GpuCard]) -> (String, String) {
    if route.share == RouteShare::None {
        return ("—".to_owned(), RouteShare::None.label().to_owned());
    }
    let place = match &route.gpu {
        Some(key) => card_name(key, gpus),
        None => "CPU".to_owned(),
    };
    let mut how = match route.share {
        RouteShare::Cpu => match route.encoder.as_ref() {
            Some(encoder) => encoder.name.clone(),
            None => RouteShare::Cpu.label().to_owned(),
        },
        share => {
            let mut how = share.label().to_owned();
            if let Some(encoder) = &route.encoder {
                how.push_str(&format!(" · {}", encoder.name));
            }
            if share == RouteShare::EncodeOnly && route.decoder == VideoDecoder::Software {
                how.push_str(" · CPU decodes");
            }
            how
        }
    };
    match route.job {
        RouteJob::ToneMap => {
            if let Some(mapper) = route.tone_mapper {
                how.push_str(&format!(" · tone map {}", mapper.label()));
            }
        }
        RouteJob::BurnIn => match route.burn_in {
            FilterPlace::Gpu => how.push_str(" · burn-in on GPU"),
            FilterPlace::Cpu => how.push_str(" · burn-in on CPU"),
            FilterPlace::None => {}
        },
        RouteJob::Convert => {}
    }
    (place, how)
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
    use spoolframe_admin_api::{GpuKind, PlanLine, Platform, ToneMapper, VideoEncoderUsed};

    fn row(display_name: &str, title: &str, plan: PlanKind) -> PlaybackRow {
        serde_json::from_value::<PlaybackRow>(serde_json::json!({
            "id": "p1",
            "userId": "local:1",
            "username": "alice",
            "displayName": display_name,
            "device": "Living room Apple TV",
            "platform": Platform::Tvos,
            "item": "plex:episode:1",
            "title": title,
            "subtitle": null,
            "durationMs": null,
            "plan": plan,
            "line": serde_json::to_value(PlanLine {
                kind: "direct".into(),
                delivery: "range".into(),
                video: "copy:h264".into(),
                audio: "copy:eac3".into(),
                subtitle: "none".into(),
            })
            .unwrap(),
            "reasons": [],
            "epoch": 0,
            "revision": 0,
            "startedAt": 0,
            "positionMs": null,
            "readToMs": null,
            "furthest": null,
            "lastReadAt": null,
            "conversion": null,
            "made": {
                "decoder": { "kind": "none" },
                "encoder": null,
                "toneMap": "none",
                "toneMapper": null,
                "burnIn": "none",
                "width": 1920,
                "height": 1080,
                "targetBitrate": null,
                "measuredBitrate": null,
            },
            "start": {
                "planMs": 12,
                "keyframes": { "from": "none" },
                "firstSegment": { "state": "notApplicable" },
            },
        }))
        .unwrap()
    }

    #[test]
    fn a_playback_says_who_what_where_and_how() {
        assert_eq!(
            playing_lines(&row("Alice", "Severance · S2 E4", PlanKind::Direct)),
            (
                "Alice · Severance S2 E4".to_owned(),
                "Living room Apple TV · direct".to_owned()
            )
        );
    }

    #[test]
    fn a_playback_without_a_display_name_names_the_username() {
        assert_eq!(
            playing_lines(&row("", "Alien", PlanKind::Transcode)).0,
            "alice · Alien"
        );
    }

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
            lacks: vec!["tone map".into()],
        }
    }

    fn route(
        job: RouteJob,
        source: &str,
        target: &str,
        gpu: Option<&str>,
        share: RouteShare,
    ) -> GpuRoute {
        GpuRoute {
            job,
            source: source.into(),
            target: target.into(),
            gpu: gpu.map(Into::into),
            share,
            decoder: VideoDecoder::Gpu { api: GpuApi::Vaapi },
            encoder: Some(VideoEncoderUsed {
                codec: target.into(),
                name: format!("{target}_vaapi"),
                gpu: Some(GpuApi::Vaapi),
            }),
            tone_map: FilterPlace::Gpu,
            tone_mapper: (job == RouteJob::ToneMap).then_some(ToneMapper::LibplaceboOwn),
            burn_in: FilterPlace::None,
        }
    }

    #[test]
    fn a_card_says_what_it_is_and_does_and_leaves_out_what_it_has_none_of() {
        let (about, lines) = card_lines(&card("vaapi:0000:03:00.0", "AMD Radeon RX 7900 XTX"));
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

    #[test]
    fn routes_group_by_job_and_name_their_card() {
        let gpus = [
            card("vaapi:0000:03:00.0", "AMD Radeon RX 7900 XTX"),
            card("vaapi:0000:6d:00.0", "AMD Radeon Graphics"),
        ];
        let routes = [
            route(
                RouteJob::Convert,
                "av1",
                "h264",
                Some("vaapi:0000:03:00.0"),
                RouteShare::Whole,
            ),
            route(RouteJob::Convert, "av1", "av1", None, RouteShare::Cpu),
            route(
                RouteJob::ToneMap,
                "hevc",
                "h264",
                Some("vaapi:0000:03:00.0"),
                RouteShare::Whole,
            ),
            route(
                RouteJob::Convert,
                "av1",
                "hevc",
                Some("vaapi:0000:6d:00.0"),
                RouteShare::None,
            ),
        ];
        let rows = route_rows(&routes);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "AV1");
        assert_eq!(rows[0].1.len(), 3);
        assert_eq!(rows[1].0, "4K HDR10 HEVC → SDR");
        assert_eq!(
            route_cell(rows[0].1[0], &gpus),
            (
                "AMD Radeon RX 7900 XTX".to_owned(),
                "all on GPU · h264_vaapi".to_owned()
            )
        );
        assert_eq!(
            route_cell(rows[0].1[1], &gpus),
            ("CPU".to_owned(), "av1_vaapi".to_owned())
        );
        assert_eq!(route_cell(rows[0].1[2], &gpus).0, "—");
        assert_eq!(
            route_cell(rows[1].1[0], &gpus).1,
            "all on GPU · h264_vaapi · tone map libplacebo (own)"
        );
    }

    #[test]
    fn two_cards_of_one_name_are_told_apart_by_place() {
        let gpus = [
            card("vaapi:0000:03:00.0", "AMD Radeon"),
            card("vaapi:0000:04:00.0", "AMD Radeon"),
        ];
        let r = route(
            RouteJob::Convert,
            "h264",
            "h264",
            Some("vaapi:0000:04:00.0"),
            RouteShare::Whole,
        );
        assert_eq!(
            route_cell(&r, &gpus).0,
            "AMD Radeon (/dev/dri/by-path/pci-vaapi:0000:04:00.0-render)"
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
