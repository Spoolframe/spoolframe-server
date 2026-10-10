//! Server › Playback (P3-15), as the WebPlayback, WebPlaybackDetail,
//! WebPlaybackStates and WebActivityPhone boards draw it: everything
//! playing, with the plan the server made and why; one playback's plan in
//! full, its reads, its conversion's command line and stderr; Stop and End
//! conversion, each asked first. The admin API has no subscriptions, so
//! the pages ask again every few seconds.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, EndedPlayback, PlanView, PlaybackDetail as Detail, PlaybackList, PlaybackRow,
};
use wasm_bindgen::JsValue;

use super::admin::sleep;
use super::catalog::Forbidden;
use super::{Alert, Route, icons, now_ms, rpc};
use crate::activity::{since, updated_line};
use crate::logic::ApiError;
use crate::logs::hours_minutes;
use crate::playback::{
    ReadsChart, at, converted_percent, decoder_text, delivery, encoder_text, ended_text,
    filter_text, furthest, made_line, out_text, output_text, plan_pill, reads_chart, start_parts,
    stream_name, why_line,
};

/// How often the pages ask: reads move by the second.
const POLL_MS: i32 = 2_000;

/// The reads chart's drawing size; it stretches to its card.
const CHART_W: f64 = 520.0;
const CHART_H: f64 = 150.0;

/// The browser's local `HH:MM` of Unix milliseconds `at`.
fn local_time(at: i64) -> String {
    let offset_min = js_sys::Date::new(&JsValue::from_f64(at as f64)).get_timezone_offset();
    hours_minutes(at - (offset_min * 60_000.0) as i64)
}

/// A polled answer: the last one and when it came, why none did, and a
/// clock ticking each second for "N s ago".
type Polled<T> = (
    Signal<Option<(T, i64)>>,
    Signal<Option<ApiError>>,
    Signal<i64>,
);

/// Ask every [`POLL_MS`] with `ask`, keeping the last answer and when it
/// came, or why none did.
fn use_polled<
    T: Clone + 'static,
    F: std::future::Future<Output = Result<T, ApiError>> + 'static,
>(
    ask: impl Fn() -> F + 'static,
) -> Polled<T> {
    let mut seen = use_signal(|| None::<(T, i64)>);
    let mut error = use_signal(|| None::<ApiError>);
    let mut clock = use_signal(now_ms);
    use_hook(move || {
        spawn(async move {
            loop {
                match ask().await {
                    Ok(answer) => {
                        seen.set(Some((answer, now_ms())));
                        error.set(None);
                    }
                    Err(e) => error.set(Some(e)),
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
    (seen, error, clock)
}

async fn list() -> Result<PlaybackList, ApiError> {
    let client = rpc::client().await?;
    client.playbacks().await.map_err(rpc::error)
}

#[component]
pub fn PlaybackPage() -> Element {
    let (seen, error, clock) = use_polled(list);
    let shown = seen();
    let live = updated_line(false, clock(), shown.as_ref().map(|(_, at)| *at));
    let count = shown.as_ref().map_or(0, |(l, _)| l.open.len());
    let forbidden = error()
        .as_ref()
        .is_some_and(|e| e.code == spoolframe_admin_api::error_code::FORBIDDEN);
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin hidden md:block", "Playback" }
                span { class: "live", role: "status",
                    span { class: if error().is_some() { "live-dot live-dot-off" } else { "live-dot" } }
                    if shown.is_some() { "{count} playing · " }
                    "{live}"
                }
            }
            p { class: "m-0 max-w-[820px] text-sm text-ctp-subtext0 max-md:hidden",
                "Everything playing on this server, with the plan the server made and why. Open one for its streams, the conversion's command line and its reads. To see the plan for something before anyone plays it, open the item in a library and use its Debug tab."
            }
            if forbidden {
                Forbidden {}
            } else if let Some(e) = error() {
                Alert { text: format!("Not updating: {}", e.explain()) }
            }
            match shown {
                None if error().is_none() => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                None => rsx! {},
                Some((list, _)) => rsx! {
                    if list.open.is_empty() {
                        section { class: "card empty max-w-[460px]",
                            span { class: "text-ctp-subtext0", icons::Play { size: 26 } }
                            b { "Nothing is playing" }
                            span { "Playbacks show up here as soon as they start." }
                        }
                    } else {
                        OpenTable { rows: list.open.clone(), now: list.now + (clock() - now_ms()).max(0) }
                        div { class: "pb-cards md:hidden",
                            for row in list.open.iter() {
                                PhoneCard { key: "{row.id}", row: row.clone() }
                            }
                        }
                    }
                    if !list.ended.is_empty() {
                        Ended { ended: list.ended.clone() }
                    }
                },
            }
        }
    }
}

/// A plan's pill.
#[component]
fn Pill(kind: spoolframe_admin_api::PlanKind) -> Element {
    let (word, tone) = plan_pill(kind);
    rsx! {
        span { class: "act-pill {tone}", span {} "{word}" }
    }
}

/// What a conversion weighs against the encode budget, and why:
/// "Weight 0.25 · audio encode".
fn weight_line(c: &spoolframe_admin_api::ConversionState) -> String {
    format!(
        "Weight {} · {}",
        spoolframe_admin_api::quarters(c.weight_quarters),
        c.load.label()
    )
}

/// The conversion's bar and its words, or a dash.
#[component]
fn ConversionCell(row: PlaybackRow) -> Element {
    let Some(c) = &row.conversion else {
        return rsx! { span { class: "cat-quiet", "—" } };
    };
    let percent = converted_percent(c.converted_ms, row.duration_ms);
    let words = match (&c.failed, c.ended, percent) {
        (Some(_), _, _) => "Failed".to_owned(),
        (None, true, _) => format!("Ended at {}", at(Some(c.converted_ms))),
        (None, false, Some(p)) => format!("{p}% converted"),
        (None, false, None) => format!("to {}", at(Some(c.converted_ms))),
    };
    rsx! {
        div { class: "flex flex-col gap-1",
            if let Some(p) = percent {
                div { class: "pb-bar",
                    span { class: if row.plan == spoolframe_admin_api::PlanKind::Transcode { "pb-bar-fill act-peach" } else { "pb-bar-fill act-blue" }, style: "width: {p}%" }
                }
            }
            span { class: if c.failed.is_some() { "text-xs text-ctp-red" } else { "text-xs text-ctp-subtext0" }, "{words}" }
            if c.paused {
                b { class: "text-xs text-ctp-yellow", "Paused by the throttle" }
            }
            span { class: "text-xs text-ctp-subtext0", {weight_line(c)} }
        }
    }
}

#[component]
fn OpenTable(rows: Vec<PlaybackRow>, now: i64) -> Element {
    let nav = navigator();
    rsx! {
        div { class: "table-card max-md:hidden",
            table { class: "table",
                thead {
                    tr {
                        th { "Playing" }
                        th { "Plan and why" }
                        th { "Delivery" }
                        th { "Position" }
                        th { "Conversion" }
                        th { "Last read" }
                    }
                }
                tbody {
                    for row in rows {
                        tr {
                            key: "{row.id}",
                            class: "opens",
                            onclick: {
                                let id = row.id.clone();
                                move |_| { nav.push(Route::PlaybackDetail { id: id.clone() }); }
                            },
                            td {
                                div { class: "who-name",
                                    Link { class: "font-bold text-ctp-text no-underline", to: Route::PlaybackDetail { id: row.id.clone() }, "{row.title}" }
                                    span { "{row.display_name} · {row.device}" }
                                }
                            }
                            td { class: "max-w-[380px]",
                                div { class: "flex flex-col items-start gap-1",
                                    Pill { kind: row.plan }
                                    span { class: "text-xs text-ctp-subtext1", "{why_line(&row)}" }
                                    span { class: "text-xs text-ctp-subtext0", "{made_line(&row)}" }
                                }
                            }
                            td {
                                div { class: "flex flex-col",
                                    span { "{delivery(&row.line.delivery)}" }
                                    span { class: "mono text-xs text-ctp-overlay0", "epoch {row.epoch}" }
                                }
                            }
                            td {
                                div { class: "flex flex-col",
                                    span { class: "mono", "{at(row.position_ms)}" }
                                    span { class: "text-xs text-ctp-subtext0", "read to {at(row.read_to_ms)}" }
                                }
                            }
                            td { class: "min-w-[150px]", ConversionCell { row: row.clone() } }
                            td {
                                match row.last_read_at {
                                    Some(read) => rsx! { span { class: if now - read > 30_000 { "text-ctp-yellow" } else { "" }, "{since(now, read)}" } },
                                    None => rsx! { span { class: "cat-quiet", "no reads yet" } },
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A playback on a phone (WebActivityPhone): what, who, where, the pill
/// and the conversion's bar.
#[component]
fn PhoneCard(row: PlaybackRow) -> Element {
    rsx! {
        Link { class: "pb-card", to: Route::PlaybackDetail { id: row.id.clone() },
            div { class: "flex items-start justify-between gap-2",
                b { "{row.title}" }
                Pill { kind: row.plan }
            }
            span { class: "text-xs text-ctp-subtext0", "{row.display_name} · {row.device} · {at(row.position_ms)}" }
            span { class: "text-xs text-ctp-subtext1", "{made_line(&row)}" }
            if row.conversion.is_some() {
                ConversionCell { row: row.clone() }
            }
        }
    }
}

#[component]
fn Ended(ended: Vec<EndedPlayback>) -> Element {
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Ended in the last hour" } }
            div { class: "pb-ended",
                for e in ended {
                    {
                        let (text, bad) = ended_text(&e.why);
                        rsx! {
                            Link { key: "{e.playback.id}", class: "pb-ended-row", to: Route::PlaybackDetail { id: e.playback.id.clone() },
                                span { class: "min-w-0 truncate", "{e.playback.display_name} · {e.playback.title}" }
                                span { Pill { kind: e.playback.plan } }
                                span { class: if bad { "min-w-0 text-ctp-red" } else { "min-w-0 text-ctp-subtext1" }, "{text}" }
                                span { class: "text-ctp-subtext0 text-right", "{local_time(e.at)}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What a confirmation is about.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ask {
    Stop,
    EndConversion,
}

#[component]
pub fn PlaybackDetail(id: String) -> Element {
    let ask_id = id.clone();
    let (seen, error, clock) = use_polled(move || {
        let id = ask_id.clone();
        async move {
            let client = rpc::client().await?;
            client.playback(id).await.map_err(rpc::error)
        }
    });
    let asking = use_signal(|| None::<Ask>);
    let forbidden = error()
        .as_ref()
        .is_some_and(|e| e.code == spoolframe_admin_api::error_code::FORBIDDEN);
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            Link { class: "flex items-center gap-1 text-sm no-underline", to: Route::PlaybackPage {},
                icons::ChevronLeft { size: 14 }
                "Playback"
            }
            if forbidden {
                Forbidden {}
            } else if let Some(e) = error() {
                Alert { text: e.explain() }
            }
            match seen() {
                None if error().is_none() => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                None => rsx! {},
                Some((detail, _)) => rsx! {
                    Page { detail: detail.clone(), now: detail.now + (clock() - now_ms()).max(0), asking }
                    if let Some(ask) = asking() {
                        Confirm { ask, detail: detail.clone(), asking }
                    }
                },
            }
        }
    }
}

#[component]
fn Page(detail: Detail, now: i64, asking: Signal<Option<Ask>>) -> Element {
    let mut asking = asking;
    let row = &detail.playback;
    let open = detail.ended.is_none();
    let can_end = open && row.conversion.as_ref().is_some_and(|c| c.running);
    let mut meta = vec![
        row.display_name.clone(),
        row.device.clone(),
        format!("started {}", local_time(row.started_at)),
    ];
    if row.conversion.is_some() {
        meta.push(format!("epoch {}", row.epoch));
    }
    meta.push(format!("playback {}", row.id));
    let failed = row.conversion.as_ref().and_then(|c| c.failed.clone());
    rsx! {
        div { class: "page-head items-start",
            div { class: "flex min-w-0 flex-col gap-1",
                h1 { class: "page-title page-title-admin m-0",
                    "{row.title}"
                    if let Some(sub) = &row.subtitle { " · {sub}" }
                }
                span { class: "text-sm text-ctp-subtext0", "{meta.join(\" · \")}" }
            }
            if open {
                div { class: "flex gap-2",
                    if can_end {
                        button { class: "btn", onclick: move |_| asking.set(Some(Ask::EndConversion)),
                            icons::CircleX { size: 15 }
                            "End conversion"
                        }
                    }
                    button { class: "btn btn-danger", onclick: move |_| asking.set(Some(Ask::Stop)),
                        icons::X { size: 15 }
                        "Stop playback"
                    }
                }
            }
        }
        if let Some(e) = &detail.ended {
            {
                let (text, bad) = ended_text(&e.why);
                rsx! {
                    div { class: if bad { "alert alert-error" } else { "alert alert-warning" }, role: "status",
                        icons::Info {}
                        span { "Ended at {local_time(e.at)}: {text}. This is how it was then." }
                    }
                }
            }
        }
        Stats { row: row.clone(), now }
        Made { row: row.clone() }
        Start { row: row.clone() }
        PlanCard { plan: detail.plan.clone(), title: "Plan" }
        div { class: "pb-two",
            Reads { detail: detail.clone() }
            Conversion { detail: detail.clone(), failed }
        }
    }
}

#[component]
fn Stat(label: String, value: String, note: String, tone: String) -> Element {
    rsx! {
        div { class: "pb-stat",
            span { class: "text-xs text-ctp-subtext0", "{label}" }
            b { class: "{tone}", "{value}" }
            span { class: "text-xs {tone}", "{note}" }
        }
    }
}

#[component]
fn Stats(row: PlaybackRow, now: i64) -> Element {
    let conversion = row.conversion.clone();
    rsx! {
        div { class: "pb-stats",
            Stat { label: "Position".to_owned(), value: at(row.position_ms), note: format!("read to {}", at(row.read_to_ms)), tone: "" }
            match &conversion {
                Some(c) => rsx! {
                    Stat {
                        label: "Converted to".to_owned(),
                        value: at(Some(c.converted_ms)),
                        note: converted_percent(c.converted_ms, row.duration_ms).map_or_else(|| "length unknown".to_owned(), |p| format!("{p} %")),
                        tone: String::new(),
                    }
                    Stat {
                        label: "Throttle".to_owned(),
                        value: if c.failed.is_some() { "Failed".to_owned() } else if c.ended { "Ended".to_owned() } else if c.paused { "Paused".to_owned() } else if c.running { "Running".to_owned() } else { "Done".to_owned() },
                        note: if c.paused { "far enough ahead of the reader".to_owned() } else if c.ended { "an admin ended the conversion".to_owned() } else { String::new() },
                        tone: if c.failed.is_some() { "text-ctp-red".to_owned() } else if c.paused || c.ended { "text-ctp-yellow".to_owned() } else { String::new() },
                    }
                    Stat {
                        label: "Encode budget".to_owned(),
                        value: spoolframe_admin_api::quarters(c.weight_quarters),
                        note: c.load.label().to_owned(),
                        tone: String::new(),
                    }
                },
                None => rsx! {
                    Stat { label: "Converted to".to_owned(), value: "—".to_owned(), note: "the file plays as it is".to_owned(), tone: "" }
                },
            }
            Stat {
                label: "Last read".to_owned(),
                value: row.last_read_at.map_or_else(|| "—".to_owned(), |r| since(now, r)),
                note: furthest(row.furthest),
                tone: String::new(),
            }
        }
    }
}

/// How the playback is made (PX-24), in the order a tester asks: how it
/// is delivered, what decodes and encodes it, where its filters run, and
/// what comes out.
#[component]
fn Made(row: PlaybackRow) -> Element {
    let made = &row.made;
    let direct = row.plan == spoolframe_admin_api::PlanKind::Direct;
    let (word, _) = plan_pill(row.plan);
    let (tone_map, tone_map_tone) = filter_text(made.tone_map);
    let (burn_in, burn_in_tone) = filter_text(made.burn_in);
    let (output, output_note) = output_text(made, direct);
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "How it is made" } }
            div { class: "card-body",
                div { class: "pb-stats",
                    Stat { label: "Delivery".to_owned(), value: word.to_owned(), note: delivery(&row.line.delivery), tone: String::new() }
                    Stat { label: "Decoder".to_owned(), value: decoder_text(made.decoder), note: String::new(), tone: String::new() }
                    Stat {
                        label: "Encoder".to_owned(),
                        value: encoder_text(made.encoder.as_ref()),
                        note: made.encoder.as_ref().map_or_else(String::new, |e| e.name.clone()),
                        tone: String::new(),
                    }
                    Stat { label: "HDR tone mapping".to_owned(), value: tone_map.to_owned(), note: String::new(), tone: tone_map_tone.to_owned() }
                    Stat { label: "Subtitle burn-in".to_owned(), value: burn_in.to_owned(), note: String::new(), tone: burn_in_tone.to_owned() }
                    Stat { label: "Output".to_owned(), value: output, note: output_note, tone: String::new() }
                }
            }
        }
    }
}

/// How long it took to start (PX-24), from the request.
#[component]
fn Start(row: PlaybackRow) -> Element {
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Start" } }
            div { class: "card-body",
                div { class: "pb-stats",
                    for (label, value, note) in start_parts(&row.start) {
                        Stat { key: "{label}", label: label.to_owned(), value, note, tone: String::new() }
                    }
                }
            }
        }
    }
}

/// A plan, a row per stream (WebPlaybackDetail, WebCatalogDebug).
#[component]
pub(super) fn PlanCard(plan: PlanView, title: &'static str) -> Element {
    let line = &plan.line;
    let summary = [
        line.kind.as_str(),
        line.delivery.as_str(),
        line.video.as_str(),
        line.audio.as_str(),
        line.subtitle.as_str(),
    ]
    .into_iter()
    .filter(|p| *p != "none")
    .collect::<Vec<_>>()
    .join(" · ");
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "{title}" }
                Pill { kind: plan.kind }
                span { class: "text-xs text-ctp-subtext0", "{delivery(&line.delivery)}" }
            }
            div { class: "overflow-x-auto",
                table { class: "table pb-plan",
                    thead {
                        tr {
                            th { "Stream" }
                            th { "In the file" }
                            th { "Out" }
                            th { "Why" }
                        }
                    }
                    tbody {
                        for (i, s) in plan.streams.iter().enumerate() {
                            {
                                let (out, tone) = out_text(&s.out);
                                rsx! {
                                    tr { key: "{i}",
                                        td { class: "font-bold whitespace-nowrap", "{stream_name(s.stream, s.index)}" }
                                        td { class: "mono text-xs", "{s.source}" }
                                        td { class: "font-semibold {tone}", "{out}" }
                                        td { class: "text-sm text-ctp-subtext1",
                                            if s.reasons.is_empty() {
                                                span { class: "cat-quiet", "Fits" }
                                            }
                                            for r in s.reasons.iter() {
                                                div { key: "{r}", "{r}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            for r in plan.reasons.iter() {
                div { key: "{r}", class: "px-[18px] pt-2 text-sm text-ctp-subtext1", "{r}" }
            }
            div { class: "pb-line mono", "{summary}" }
        }
    }
}

#[component]
fn Reads(detail: Detail) -> Element {
    let window_ms = i64::from(detail.reads_window_secs) * 1000;
    let chart: Option<ReadsChart> =
        reads_chart(&detail.reads, detail.now, window_ms, CHART_W, CHART_H);
    let minutes = detail.reads_window_secs / 60;
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Reads" } }
            div { class: "card-body",
                match chart {
                    None => rsx! { span { class: "text-sm text-ctp-subtext0", "Not enough reads in the last {minutes} min to draw." } },
                    Some(c) => rsx! {
                        svg {
                            class: "pb-chart",
                            view_box: "0 0 {CHART_W} {CHART_H}",
                            preserve_aspect_ratio: "none",
                            role: "img",
                            "aria-label": "Read position and converted-to over the last {minutes} minutes, up to {at(Some(c.top_ms))}",
                            line { class: "pb-grid", x1: "0", x2: "{CHART_W}", y1: "{CHART_H / 3.0}", y2: "{CHART_H / 3.0}" }
                            line { class: "pb-grid", x1: "0", x2: "{CHART_W}", y1: "{CHART_H * 2.0 / 3.0}", y2: "{CHART_H * 2.0 / 3.0}" }
                            if let Some(converted) = &c.converted {
                                polyline { class: "act-line act-peach", points: "{converted}" }
                            }
                            polyline { class: "act-line act-teal", points: "{c.read}" }
                        }
                        div { class: "flex flex-wrap items-center gap-4 text-xs text-ctp-subtext0",
                            span { class: "flex items-center gap-1.5", span { class: "pb-key act-teal" } "Read position" }
                            if c.converted.is_some() {
                                span { class: "flex items-center gap-1.5", span { class: "pb-key act-peach" } "Converted to" }
                            }
                            span { class: "ml-auto", "up to {at(Some(c.top_ms))} · last {minutes} min" }
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn Conversion(detail: Detail, failed: Option<String>) -> Element {
    let mut copied = use_signal(|| false);
    let Some(command) = detail.command.clone() else {
        return rsx! {
            section { class: "card",
                div { class: "card-head", h2 { "Conversion" } }
                div { class: "card-body",
                    span { class: "text-sm text-ctp-subtext0", "None: the file is read as it is, so no ffmpeg runs for it." }
                }
            }
        };
    };
    let copy = {
        let command = command.clone();
        move |_| {
            if let Some(window) = web_sys::window() {
                let _ = window.navigator().clipboard().write_text(&command);
                copied.set(true);
            }
        }
    };
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "Conversion" }
                button { class: "btn h-[30px]", onclick: copy,
                    if copied() { icons::Check { size: 14 } } else { icons::LinkChain { size: 14 } }
                    if copied() { "Copied" } else { "Copy" }
                }
            }
            div { class: "card-body",
                pre { class: "pb-code", "{command}" }
                span { class: "text-xs text-ctp-subtext0", "Paths are shown; tokens never are." }
                if let Some(reason) = &failed {
                    div { class: "pb-failed", role: "alert",
                        icons::CircleX { size: 18 }
                        span { class: "flex flex-col",
                            b { "{reason}" }
                            span { class: "text-ctp-text", "The last stderr lines show why." }
                        }
                    }
                }
                b { class: "text-sm", "Last stderr lines" }
                if detail.stderr.is_empty() {
                    span { class: "text-sm text-ctp-subtext0", "ffmpeg has written nothing to stderr: it logs errors only." }
                } else {
                    pre { class: if failed.is_some() { "pb-code pb-code-bad" } else { "pb-code" },
                        "{detail.stderr.join(\"\\n\")}"
                    }
                }
            }
        }
    }
}

/// Stop and End conversion ask first (WebPlaybackStates, 1 and 2).
#[component]
fn Confirm(ask: Ask, detail: Detail, asking: Signal<Option<Ask>>) -> Element {
    let mut asking = asking;
    let mut busy = use_signal(|| false);
    let mut failed = use_signal(|| None::<String>);
    let row = detail.playback.clone();
    let (title, text, action) = match ask {
        Ask::Stop => (
            format!("Stop {}'s playback?", row.display_name),
            format!(
                "{} shows that the playback ended. Its progress is kept.",
                row.device
            ),
            "Stop playback",
        ),
        Ask::EndConversion => (
            "End this conversion?".to_owned(),
            "ffmpeg stops and its slot is free. The playback carries on until it runs out of converted video, then ends.".to_owned(),
            "End conversion",
        ),
    };
    let id = row.id.clone();
    let go = move |_| {
        busy.set(true);
        let id = id.clone();
        spawn(async move {
            let done: Result<(), ApiError> = async {
                let client = rpc::client().await?;
                match ask {
                    Ask::Stop => client.playback_stop(id).await,
                    Ask::EndConversion => client.playback_end_conversion(id).await,
                }
                .map_err(rpc::error)
            }
            .await;
            busy.set(false);
            match done {
                Ok(()) => asking.set(None),
                Err(e) => failed.set(Some(e.explain())),
            }
        });
    };
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: if ask == Ask::Stop { "dialog-head dialog-head-danger" } else { "dialog-head" },
                    match ask {
                        Ask::Stop => rsx! { icons::X { size: 22 } },
                        Ask::EndConversion => rsx! { icons::CircleX { size: 22 } },
                    }
                    h1 { "{title}" }
                }
                p { "{text}" }
                if let Some(why) = failed() {
                    Alert { text: why }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| asking.set(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: go, "{action}" }
                }
            }
        }
    }
}
