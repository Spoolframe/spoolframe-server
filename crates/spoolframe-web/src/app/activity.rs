//! Server › Activity (P3-14), as the WebActivity and WebActivityPhone
//! boards lay it out: a card for each part of the server, its number now,
//! the last hour as a small graph, and the counts behind it. The admin API
//! has no subscriptions, so the page asks again every few seconds.

use dioxus::prelude::*;
use spoolframe_admin_api::{Activity, ActivityPoint, AdminApiClient, TelemetryStatus};

use super::admin::sleep;
use super::{Alert, Route, icons, now_ms, rpc};
use crate::activity::{
    Spark, bytes_per_sec, last_sync, megabytes, memory_rows, output_speed, paired,
    peer_requests_line, percent, plex_line, proxied_line, rate, series, session_rows, sparkline,
    updated_line, uptime, work_line,
};
use crate::logic::{ApiError, thousands};
use crate::telemetry::{export_state, export_summary};

/// How often the page asks: more often than the server samples (every
/// 10 s), so a new point shows soon after it is made.
const POLL_MS: i32 = 5_000;

/// The graphs' drawing size; they stretch to their card.
const SPARK_W: f64 = 220.0;
const SPARK_H: f64 = 44.0;

/// The last answers, and when they came.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    activity: Activity,
    telemetry: Option<TelemetryStatus>,
    at: i64,
}

async fn ask() -> Result<(Activity, Option<TelemetryStatus>), ApiError> {
    let client = rpc::client().await?;
    let activity = client.activity().await.map_err(rpc::error)?;
    // The Telemetry card goes without, rather than the whole page.
    let telemetry = client.telemetry_status().await.ok();
    Ok((activity, telemetry))
}

/// Ask once, and keep what came, or why nothing did.
async fn refresh(mut seen: Signal<Option<Seen>>, mut error: Signal<Option<ApiError>>) {
    match ask().await {
        Ok((activity, telemetry)) => {
            seen.set(Some(Seen {
                activity,
                telemetry,
                at: now_ms(),
            }));
            error.set(None);
        }
        Err(e) => error.set(Some(e)),
    }
}

#[component]
pub fn ActivityPage() -> Element {
    let seen = use_signal(|| None::<Seen>);
    let error = use_signal(|| None::<ApiError>);
    let mut paused = use_signal(|| false);
    // Ticks each second, so "updated N s ago" counts on between answers.
    let mut clock = use_signal(now_ms);
    let version = use_resource(|| async {
        let client = rpc::client().await?;
        client
            .server_status()
            .await
            .map(|s| s.version)
            .map_err(rpc::error)
    });
    use_hook(move || {
        spawn(async move {
            loop {
                if !*paused.peek() {
                    refresh(seen, error).await;
                }
                sleep(POLL_MS).await;
            }
        });
    });
    use_hook(move || {
        spawn(async move {
            loop {
                sleep(1000).await;
                clock.set(now_ms());
            }
        });
    });
    let toggle = move |_| {
        let resuming = paused();
        paused.toggle();
        if resuming {
            spawn(async move { refresh(seen, error).await });
        }
    };
    let shown = seen();
    let live = updated_line(paused(), clock(), shown.as_ref().map(|s| s.at));
    let version = version.read().clone().and_then(Result::ok);
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin hidden md:block", "Activity" }
                div { class: "flex items-center gap-3",
                    span { class: "live", role: "status",
                        span { class: if paused() || error().is_some() { "live-dot live-dot-off" } else { "live-dot" } }
                        "{live}"
                    }
                    button { class: "btn h-[30px]", onclick: toggle,
                        if paused() {
                            icons::Play { size: 15 }
                            "Resume"
                        } else {
                            icons::Pause {}
                            "Pause"
                        }
                    }
                }
            }
            p { class: "m-0 max-w-[820px] text-sm text-ctp-subtext0 max-md:hidden",
                "What the server is doing now. Graphs cover the last hour, kept in memory: they start empty when the server restarts. "
                Link { class: "no-underline", to: Route::Settings { tab: "telemetry".into() }, "Export to Prometheus or OpenTelemetry" }
            }
            if let Some(e) = error() {
                Alert { text: format!("Not updating: {}", e.explain()) }
            }
            match shown {
                None if error().is_none() => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                None => rsx! {},
                Some(seen) => rsx! {
                    Cards { seen, now: clock(), version }
                },
            }
        }
    }
}

/// The graph of `pick` over the last hour.
fn spark(a: &Activity, now: i64, pick: impl Fn(&ActivityPoint) -> f64) -> Option<Spark> {
    sparkline(&series(&a.history, &a.now, pick), now, SPARK_W, SPARK_H)
}

#[component]
fn Cards(seen: Seen, now: i64, version: Option<String>) -> Element {
    let a = &seen.activity;
    let p = &a.now;
    let t = &a.totals;
    let playbacks = p.playbacks_direct + p.playbacks_remux + p.playbacks_transcode;
    let (read_rate, read_unit) = bytes_per_sec(p.media_bytes_per_sec);
    let telemetry = seen.telemetry.clone();
    let exporting = telemetry.as_ref().is_some_and(|s| s.otlp.is_some());
    let scraped = telemetry.as_ref().is_some_and(|s| s.metrics.enabled);
    rsx! {
        div { class: "act-grid",
            Card {
                title: "Playbacks",
                value: "{playbacks}",
                unit: "active",
                tone: "act-green",
                spark: spark(a, now, |p| (p.playbacks_direct + p.playbacks_remux + p.playbacks_transcode) as f64),
                Line { label: rsx! { span { class: "act-pill act-green", span {} "Direct" } }, "{p.playbacks_direct}" }
                Line { label: rsx! { span { class: "act-pill act-blue", span {} "Remux" } }, "{p.playbacks_remux}" }
                Line { label: rsx! { span { class: "act-pill act-peach", span {} "Transcode" } }, "{p.playbacks_transcode}" }
                Line { label: rsx! { "Started · ended" }, "{thousands(t.playbacks_started)} · {thousands(t.playbacks_ended)}" }
                Line { label: rsx! { "Expired · failed" },
                    "{thousands(t.playbacks_expired)} · "
                    span { class: if t.playbacks_failed > 0 { "text-ctp-red" } else { "" }, "{thousands(t.playbacks_failed)}" }
                }
            }
            Card {
                title: "Conversions",
                value: "{p.conversions_running}",
                unit: "running",
                tone: "act-peach",
                spark: spark(a, now, |p| p.conversions_running as f64),
                Line { label: rsx! { "Paused by the throttle" }, "{p.conversions_paused}" }
                Line { label: rsx! { "Output speed" }, "{output_speed(p.output_rate, p.conversions_running)}" }
                Line { label: rsx! { "Refused at the job limit" }, "{thousands(t.conversions_refused)}" }
                Line { label: rsx! { "Stalls ended" }, "{thousands(t.conversions_stalled)}" }
            }
            Card {
                title: "Media reads",
                value: read_rate,
                unit: read_unit,
                tone: "act-blue",
                spark: spark(a, now, |p| p.media_bytes_per_sec),
                Line { label: rsx! { "Range reads" }, "{rate(p.media_reads_per_sec)} /s" }
                Line { label: rsx! { "Subtitle reads" }, "{rate(p.subtitle_reads_per_sec * 60.0)} /min" }
                Line { label: rsx! { "Artwork requests" }, "{rate(p.artwork_requests_per_sec * 60.0)} /min" }
                Line { label: rsx! { "Artwork cache hits" }, "{percent(p.artwork_hit_rate)}" }
            }
            Card {
                title: "Connections",
                value: "{p.connections_app + p.connections_web}",
                unit: "open",
                tone: "act-teal",
                spark: spark(a, now, |p| (p.connections_app + p.connections_web) as f64),
                Line { label: rsx! { "Apps" }, "{p.connections_app}" }
                Line { label: rsx! { "Web" }, "{p.connections_web}" }
                for (label, n) in session_rows(&a.sessions) {
                    Line { key: "{label}", label: rsx! { "{label}" }, "{n}" }
                }
            }
            Card {
                title: "Plex",
                value: rate(p.plex_requests_per_sec * 60.0),
                unit: "requests/min",
                tone: "act-yellow",
                spark: spark(a, now, |p| p.plex_requests_per_sec * 60.0),
                for server in a.plex.iter() {
                    Line { key: "{server.id}", label: rsx! { "{server.name}" },
                        {
                            let (text, bad) = plex_line(server);
                            rsx! { span { class: if bad { "text-ctp-red" } else { "" }, "{text}" } }
                        }
                    }
                }
                Line { label: rsx! { "Watch sync" },
                    "{thousands(t.sync_runs_ok)} runs · "
                    span { class: if t.sync_runs_failed > 0 { "text-ctp-red" } else { "" }, "{thousands(t.sync_runs_failed)} failed" }
                }
            }
            // Only while the server has federation on (P8-01): the
            // snapshot carries none otherwise.
            if let Some(f) = &a.federation {
                {
                    let (value, unit) = paired(f);
                    rsx! {
                        Card {
                            title: "Federation",
                            value,
                            unit,
                            tone: "act-blue",
                            spark: spark(a, now, |p| p.federation_proxied_bytes_per_sec),
                            Line { label: rsx! { "Syncs" },
                                "{thousands(f.syncs_ok)} runs · "
                                span { class: if f.syncs_failed > 0 { "text-ctp-red" } else { "" }, "{thousands(f.syncs_failed)} failed" }
                            }
                            Line { label: rsx! { "Last sync" }, "{last_sync(now, f.last_sync_at)}" }
                            Line { label: rsx! { "Requests from pairs" }, "{peer_requests_line(p)}" }
                            Line { label: rsx! { "Proxied streams" }, "{proxied_line(f.proxied_playbacks, p.federation_proxied_bytes_per_sec)}" }
                            Line { label: rsx! { "Plays · ours · theirs" }, "{thousands(f.plays_ours)} · {thousands(f.plays_theirs)}" }
                            Line { label: rsx! { "Cache" }, "{megabytes(f.cache_bytes)} MB" }
                        }
                    }
                }
            }
            Card {
                title: "Background work",
                value: "{p.previews_queued + p.ocr_queued}",
                unit: "queued",
                tone: "act-mauve",
                spark: spark(a, now, |p| (p.previews_queued + p.ocr_queued) as f64),
                Line { label: rsx! { "Previews" }, "{work_line(p.previews_queued, t.previews_done, t.previews_failed)}" }
                Line { label: rsx! { "Subtitle reading (OCR)" }, "{work_line(p.ocr_queued, t.ocr_done, t.ocr_failed)}" }
            }
            Card {
                title: "Process",
                value: p.memory_bytes.map(megabytes).unwrap_or_else(|| "—".into()),
                unit: "MB memory",
                tone: "act-grey",
                spark: spark(a, now, |p| p.memory_bytes.unwrap_or(0) as f64),
                if let Some(memory) = &p.memory {
                    for (label, value) in memory_rows(memory) {
                        Line { key: "{label}", label: rsx! { "{label}" }, "{value}" }
                    }
                }
                Line { label: rsx! { "Open files" }, {p.open_files.map(thousands).unwrap_or_else(|| "—".into())} }
                Line { label: rsx! { "Uptime" }, "{uptime(t.uptime_secs)}" }
                if let Some(v) = version {
                    Line { label: rsx! { "Version" }, span { class: "mono-value text-xs", "{v}" } }
                }
            }
            Card {
                title: "Telemetry export",
                value: if exporting || scraped { "On" } else { "Off" },
                unit: "",
                tone: "act-grey",
                spark: None,
                Line { label: rsx! { "Prometheus /metrics" }, if scraped { "on" } else { "off" } }
                Line { label: rsx! { "OpenTelemetry" },
                    if let Some(t) = &telemetry {
                        {
                            let (text, bad) = export_summary(&export_state(t.otlp.as_ref()), now);
                            rsx! { span { class: if bad { "text-ctp-red" } else { "" }, "{text}" } }
                        }
                    } else {
                        "—"
                    }
                }
                div { class: "pt-1 text-xs",
                    Link { class: "no-underline", to: Route::Settings { tab: "telemetry".into() }, "Set up in Settings › Telemetry" }
                }
            }
        }
    }
}

/// One card: its title, the number now, the graph, and its rows. On a
/// phone, the title, the number and the graph only.
#[component]
fn Card(
    title: &'static str,
    value: String,
    unit: String,
    tone: &'static str,
    spark: Option<Spark>,
    children: Element,
) -> Element {
    rsx! {
        section { class: "act-card",
            div { class: "act-card-head",
                h2 { "{title}" }
                span { "last hour" }
            }
            div { class: "act-big",
                b { "{value}" }
                span { "{unit}" }
            }
            svg {
                class: "act-spark {tone}",
                view_box: "0 0 {SPARK_W} {SPARK_H}",
                preserve_aspect_ratio: "none",
                "aria-hidden": "true",
                if let Some(s) = spark {
                    polygon { class: "act-area", points: "{s.area}" }
                    polyline { class: "act-line", points: "{s.line}" }
                }
            }
            div { class: "act-rows", {children} }
        }
    }
}

/// A card's row: what, and how many.
#[component]
fn Line(label: Element, children: Element) -> Element {
    rsx! {
        div { class: "act-row",
            span { class: "text-ctp-subtext0", {label} }
            span { class: "act-row-value", {children} }
        }
    }
}
