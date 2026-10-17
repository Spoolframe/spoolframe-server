//! An item's Debug tab (P3-15), as the WebCatalogDebug board draws it:
//! its file as the server's ffprobe sees it; "Plan for…" a device (one
//! connected now, or a stored profile), tracks and a picture-subtitle
//! mode, showing the plan the server would make without playing
//! anything; and its subtitle text and frame previews as the caches have
//! them.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, ItemDebug, PictureSubtitles, PlanFor, Probe, StreamKind,
};

use super::playback::PlanCard;
use super::{Alert, icons, rpc};
use crate::catalog::{bytes, clock};
use crate::logic::ApiError;
use crate::playback::{bitrate_text, cache_text, previews_text};

async fn load(id: String) -> Result<ItemDebug, ApiError> {
    let client = rpc::client().await?;
    client.catalog_debug(id).await.map_err(rpc::error)
}

#[component]
pub(super) fn DebugTab(id: String) -> Element {
    let mut seen = use_resource(use_reactive((&id,), |(id,)| load(id)));
    let current = seen.read().clone();
    match current {
        None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Looking at the file…" } },
        Some(Err(e)) => rsx! { Alert { text: e.explain() } },
        Some(Ok(debug)) => rsx! {
            div { class: "flex flex-col gap-4",
                match &debug.probe {
                    Some(probe) => rsx! { ProbeCard { probe: probe.clone(), on_refresh: move |_| seen.restart() } },
                    None => rsx! {
                        Alert { text: format!("The file could not be probed: {}", debug.probe_error.clone().unwrap_or_default()) }
                    },
                }
                if let Some(probe) = &debug.probe {
                    PlanForCard { id: id.clone(), debug: debug.clone(), probe: probe.clone() }
                }
                Caches { debug: debug.clone() }
                super::subtitle_timing::SubtitleTimingCard { id: id.clone() }
            }
        },
    }
}

fn kind_word(kind: StreamKind) -> &'static str {
    match kind {
        StreamKind::Container => "Container",
        StreamKind::Video => "Video",
        StreamKind::Audio => "Audio",
        StreamKind::Subtitle => "Subtitle",
    }
}

#[component]
fn ProbeCard(probe: Probe, on_refresh: EventHandler<()>) -> Element {
    let mut head = vec![probe.container.clone()];
    if let Some(d) = probe.duration_ms {
        head.push(clock(d));
    }
    if let Some(s) = probe.size {
        head.push(bytes(s));
    }
    if let Some(b) = probe.bit_rate {
        head.push(bitrate_text(b));
    }
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "Probe" }
                span { class: "text-xs text-ctp-subtext0", "{head.join(\" · \")}" }
                button { class: "btn h-[30px]", onclick: move |_| on_refresh.call(()), icons::RotateCcw { size: 14 } "Look again" }
            }
            div { class: "px-[18px] pt-3 mono text-xs text-ctp-subtext1 break-all", "{probe.path}" }
            div { class: "overflow-x-auto",
                table { class: "table",
                    thead {
                        tr {
                            th { "#" }
                            th { "Kind" }
                            th { "Track" }
                            th { "Codec" }
                            th { "Details" }
                            th { "Language" }
                            th { "Role" }
                        }
                    }
                    tbody {
                        for (i, s) in probe.streams.iter().enumerate() {
                            tr { key: "{i}",
                                td { class: "mono text-xs", {s.index.map_or_else(|| "file".to_owned(), |n| n.to_string())} }
                                td { "{kind_word(s.kind)}" }
                                td { class: "mono text-xs", "{s.track}" }
                                td { class: "mono", "{s.codec}" }
                                td { class: "text-sm",
                                    "{s.details}"
                                    if let Some(t) = &s.title { span { class: "cat-quiet", " · {t}" } }
                                    if let Some(f) = &s.file { span { class: "cat-quiet", " · {f}" } }
                                }
                                td { {s.language.clone().unwrap_or_else(|| "—".to_owned())} }
                                td { class: "text-sm",
                                    {s.role.clone().unwrap_or_default()}
                                    if s.default { span { class: "cat-quiet", " · default" } }
                                    if s.forced && s.role.as_deref() != Some("forced") { span { class: "cat-quiet", " · forced" } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A track as the selects name it: `1 · dts 7.1 eng`.
fn track_label(s: &spoolframe_admin_api::ProbeStream) -> String {
    let mut parts = vec![s.codec.clone()];
    if s.kind == StreamKind::Audio && !s.details.is_empty() {
        parts.push(s.details.replace(" · ", " "));
    }
    if let Some(l) = &s.language {
        parts.push(l.clone());
    }
    if let Some(r) = s.role.as_deref().filter(|r| *r != "main" && *r != "full") {
        parts.push(r.to_owned());
    }
    format!("{} · {}", s.track, parts.join(" "))
}

#[component]
fn PlanForCard(id: String, debug: ItemDebug, probe: Probe) -> Element {
    let audio_tracks: Vec<_> = probe
        .streams
        .iter()
        .filter(|s| s.kind == StreamKind::Audio)
        .cloned()
        .collect();
    let subtitle_tracks: Vec<_> = probe
        .streams
        .iter()
        .filter(|s| s.kind == StreamKind::Subtitle)
        .cloned()
        .collect();
    let first_device = debug
        .devices
        .first()
        .map(|d| d.id.clone())
        .unwrap_or_default();
    let default_audio = audio_tracks
        .iter()
        .find(|a| a.default)
        .or(audio_tracks.first())
        .map(|a| a.track);
    let mut device = use_signal(move || first_device);
    let mut audio = use_signal(move || default_audio);
    let mut subtitle = use_signal(|| None::<u32>);
    let mut pictures = use_signal(|| PictureSubtitles::Auto);
    let plan = use_resource(move || {
        let id = id.clone();
        let request = PlanFor {
            device: device(),
            audio: audio(),
            subtitle: subtitle(),
            pictures: pictures(),
        };
        async move {
            let client = rpc::client().await?;
            client
                .catalog_plan_for(id, request)
                .await
                .map_err(rpc::error)
        }
    });
    let note = debug
        .devices
        .iter()
        .find(|d| d.id == device())
        .map(|d| d.note.clone())
        .unwrap_or_default();
    let answer = plan.read().clone();
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Plan for…" } }
            div { class: "card-body",
                div { class: "dbg-form",
                    label { class: "dbg-field",
                        span { "Device" }
                        select { class: "pref-select dbg-select", onchange: move |e| device.set(e.value()),
                            for d in debug.devices.iter() {
                                option { key: "{d.id}", value: "{d.id}", selected: d.id == device(),
                                    if d.connected { "{d.name} (connected)" } else { "{d.name}" }
                                }
                            }
                        }
                    }
                    label { class: "dbg-field",
                        span { "Audio" }
                        select { class: "pref-select dbg-select", onchange: move |e| audio.set(e.value().parse().ok()),
                            if audio_tracks.is_empty() {
                                option { value: "", "None in the file" }
                            }
                            for a in audio_tracks.iter() {
                                option { key: "{a.track}", value: "{a.track}", selected: audio() == Some(a.track), "{track_label(a)}" }
                            }
                        }
                    }
                    label { class: "dbg-field",
                        span { "Subtitles" }
                        select { class: "pref-select dbg-select", onchange: move |e| subtitle.set(e.value().parse().ok()),
                            option { value: "", selected: subtitle().is_none(), "Off" }
                            for s in subtitle_tracks.iter() {
                                option { key: "{s.track}", value: "{s.track}", selected: subtitle() == Some(s.track), "{track_label(s)}" }
                            }
                        }
                    }
                    label { class: "dbg-field",
                        span { "Picture subtitles" }
                        select {
                            class: "pref-select dbg-select",
                            onchange: move |e| pictures.set(match e.value().as_str() {
                                "text" => PictureSubtitles::Text,
                                "burn" => PictureSubtitles::BurnIn,
                                _ => PictureSubtitles::Auto,
                            }),
                            option { value: "auto", selected: pictures() == PictureSubtitles::Auto, "Auto" }
                            option { value: "text", selected: pictures() == PictureSubtitles::Text, "Text" }
                            option { value: "burn", selected: pictures() == PictureSubtitles::BurnIn, "Burn in" }
                        }
                    }
                }
                span { class: "text-xs text-ctp-subtext0",
                    "{note} Nothing plays: this is the plan the server would make."
                }
            }
        }
        match answer {
            None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Planning…" } },
            Some(Err(e)) => rsx! { Alert { text: e.explain() } },
            Some(Ok(view)) => rsx! { PlanCard { plan: view, title: "Plan" } },
        }
    }
}

#[component]
fn Caches(debug: ItemDebug) -> Element {
    let (previews, previews_tone) = previews_text(&debug.previews);
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Subtitles and previews" } }
            div { class: "dbg-rows",
                for (i, s) in debug.subtitles.iter().enumerate() {
                    {
                        let (state, tone) = cache_text(&s.state);
                        let how = match (&s.to, s.ocr) {
                            (Some(to), true) => format!("read as text → {}", crate::playback::codec(to)),
                            (Some(to), false) => format!("converted → {}", crate::playback::codec(to)),
                            (None, true) => "read as text".to_owned(),
                            (None, false) => "converted".to_owned(),
                        };
                        rsx! {
                            div { key: "{i}", class: "dbg-row",
                                span { "Subtitle {s.track} · {s.label} · {how}" }
                                span { class: "{tone}", "{state}" }
                            }
                        }
                    }
                }
                if debug.subtitles.is_empty() {
                    div { class: "dbg-row",
                        span { class: "cat-quiet", "No subtitle text made or to make" }
                    }
                }
                div { class: "dbg-row",
                    span { "Frame previews" }
                    span { class: "{previews_tone}", "{previews}" }
                }
            }
        }
    }
}
