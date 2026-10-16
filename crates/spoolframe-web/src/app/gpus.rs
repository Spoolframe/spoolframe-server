//! The Server page's GPUs (PGPU-05): each card encodes run on, with what
//! its start line says; the host's other cards and why they do nothing;
//! the entries that took none; and where typical conversions run, each
//! cell the order the server's own pick tries the cards in.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, TranscodingStatus};

use super::admin::sleep;
use super::{Alert, Route, icons, rpc};
use crate::gpus::{
    TARGETS, card_lines, codec_label, filters_off_matter, other_why, pick_lines, route_cell,
    route_rows,
};

/// How often the card asks again: the jobs on each GPU change by the
/// minute.
const REFRESH_MS: i32 = 15_000;

#[component]
pub fn Gpus() -> Element {
    let mut tick = use_signal(|| 0u32);
    use_hook(move || {
        spawn(async move {
            loop {
                sleep(REFRESH_MS).await;
                tick += 1;
            }
        });
    });
    let status = use_resource(move || async move {
        let _ = tick();
        let client = rpc::client().await?;
        client.server_status().await.map_err(rpc::error)
    });
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::Chip {}
                h2 { "GPUs" }
                Link { class: "ml-auto text-[13px]", to: Route::Settings { tab: "playback".into() }, "Settings" }
            }
            div { class: "card-body",
                match &*status.read() {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(s)) => rsx! { GpusBody { transcoding: s.transcoding.clone() } },
                }
            }
        }
    }
}

#[component]
fn GpusBody(transcoding: TranscodingStatus) -> Element {
    let t = transcoding;
    let rows = route_rows(&t.routes);
    let picks = pick_lines(&t.picks, &t.routes, &t.gpus);
    rsx! {
        if t.gpus.is_empty() {
            p { class: "m-0 text-sm text-ctp-subtext0", "No GPU encodes here: every conversion runs in software." }
        }
        div { class: "gpu-grid",
            for gpu in t.gpus.iter() {
                {
                    let (about, lines) = card_lines(gpu);
                    rsx! {
                        div { key: "{gpu.key}", class: "gpu-card",
                            b { "{gpu.name}" }
                            span { class: "row-sub", "{about}" }
                            span { class: "row-sub mono-value", "{gpu.place} · {gpu.classified}" }
                            dl {
                                for (label, value) in lines {
                                    dt { "{label}" }
                                    dd { class: if label == "Lacks" { "text-ctp-yellow" } else { "" }, "{value}" }
                                }
                            }
                        }
                    }
                }
            }
        }
        for other in t.other_gpus.iter() {
            div { key: "{other.pci}", class: "row",
                span { class: "row-what",
                    b { "{other.name}" }
                    span { class: "row-sub", "{other.pci} · {other.driver} · {other.kind.label()} · {other_why(other)}" }
                }
                span { class: "row-do", span { class: "state text-ctp-subtext0", "Not used" } }
            }
        }
        for note in t.gpu_notes.iter() {
            p { key: "{note}", class: "m-0 max-w-[820px] text-xs text-ctp-subtext0", "{note}" }
        }
        if !picks.is_empty() {
            div { class: "row-label", "A 4K HDR10 film becomes" }
            for line in picks.iter() {
                p { key: "{line}", class: "m-0 text-sm", "{line}" }
            }
            p { class: "m-0 max-w-[820px] text-xs text-ctp-subtext0",
                "The first codec of Output codecs the device plays, HDR kept where its screen shows HDR. Set in "
                Link { to: Route::Settings { tab: "playback".into() }, "Settings › Playback" }
                "."
            }
        }
        if !rows.is_empty() {
            div { class: "row-label", "Where conversions run" }
            div { class: "table-card",
                table { class: "table gpu-routes",
                    thead {
                        tr {
                            th { "Source" }
                            for target in TARGETS {
                                th { "→ {codec_label(target)}" }
                            }
                        }
                    }
                    tbody {
                        for (label, cells) in rows.iter() {
                            tr { key: "{label}",
                                td { "{label}" }
                                for route in cells.iter() {
                                    if let Some(route) = route {
                                        {
                                        let cell = route_cell(route, &t.gpus);
                                        rsx! {
                                            td {
                                                div {
                                                    b { "{cell.first}" }
                                                    for next in cell.then.iter() {
                                                        span { class: "text-ctp-subtext0", " → {next}" }
                                                    }
                                                }
                                                div { class: "text-xs text-ctp-subtext0", "{cell.how}" }
                                                if let Some(why) = cell.why {
                                                    div { class: "text-xs text-ctp-yellow", "{why}" }
                                                }
                                            }
                                        }
                                        }
                                    } else {
                                        td { class: "text-ctp-subtext0", "—" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            p { class: "m-0 max-w-[820px] text-xs text-ctp-subtext0",
                "First choice; when it's busy, the next. \"CPU\": every card that encodes H.264 is full, so a new playback is encoded in software while the CPU budget (transcode.max_jobs) has room; software encodes H.264 only. \"next codec\": every card that encodes it is full, so a new playback takes the next of Output codecs. Each cell is the server's own pick, for a 1080p source to 720p, a 4K HDR10 one to 1080p SDR or with its HDR kept, and a 1080p one with picture subtitles."
            }
            if filters_off_matter(&t.routes) {
                p { class: "m-0 max-w-[820px] text-xs text-ctp-subtext0",
                    "GPU filters are off, so these tone maps and burn-ins run on the CPU. Turn them on in "
                    Link { to: Route::Settings { tab: "playback".into() }, "Settings › Playback" }
                    "."
                }
            }
        }
    }
}
