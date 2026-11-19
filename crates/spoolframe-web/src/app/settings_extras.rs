//! What an area page shows besides its fields: how the OTLP export goes
//! (Monitoring), and the day's subtitle downloads (Subtitles).

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, TelemetryStatus};
use wasm_bindgen::JsValue;

use super::{icons, now_ms, rpc};
use crate::logic::ApiError;
use crate::logs::hours_minutes;
use crate::subtitle_search::{pair_usage, site_usage_line, user_usage};
use crate::telemetry::{ExportState, dropping_line, export_state, exporting_line};

async fn load_status() -> Result<TelemetryStatus, ApiError> {
    let client = rpc::client().await?;
    client.telemetry_status().await.map_err(rpc::error)
}

/// `at`, Unix milliseconds, as the browser's local `HH:MM`.
fn local_hours_minutes(at: i64) -> String {
    let offset_min = js_sys::Date::new(&JsValue::from_f64(at as f64)).get_timezone_offset();
    hours_minutes(at - (offset_min * 60_000.0) as i64)
}

/// Monitoring: how the saved OTLP export goes, with "Try now".
#[component]
pub(super) fn OtlpStatus() -> Element {
    let mut status = use_signal(|| None::<Result<TelemetryStatus, ApiError>>);
    use_future(move || async move {
        status.set(Some(load_status().await));
    });
    let otlp = match &*status.read() {
        Some(Ok(t)) => t.otlp.clone(),
        _ => return rsx! {},
    };
    rsx! {
        section { class: "card", aria_label: "Export",
            div { class: "card-body",
                ExportLine {
                    state: export_state(otlp.as_ref()),
                    on_try: move |_| {
                        spawn(async move { status.set(Some(load_status().await)) });
                    },
                }
            }
        }
    }
}

/// Settings › Subtitles: the day's downloads, each site's (with its
/// account's count as the site said it) and each user's against the
/// daily limit. Nothing without playback to look at.
#[component]
pub(super) fn UsageToday() -> Element {
    let usage = use_resource(|| async move {
        let client = rpc::client().await?;
        client.subtitle_usage().await.map_err(rpc::error)
    });
    let Some(Ok(usage)) = &*usage.read() else {
        return rsx! {};
    };
    let now = super::now();
    let limit = usage.daily_limit_per_user;
    rsx! {
        div { class: "flex flex-col gap-1.5",
            h3 { class: "m-0 text-sm font-semibold", "Today" }
            span { class: "text-xs text-ctp-subtext0", "The last 24 hours, as the daily limit counts them. A removed download still counts." }
            for line in usage.sites.iter().map(|s| site_usage_line(s, now)) {
                p { key: "{line}", class: "m-0 text-sm", "{line}" }
            }
            if usage.users.is_empty() {
                p { class: "m-0 text-sm text-ctp-subtext0", "No one downloaded a subtitle." }
            } else {
                div { class: "dbg-rows",
                    for (i, user) in usage.users.iter().enumerate() {
                        div { key: "{i}", class: "dbg-row",
                            span { class: "cat-break", "{user.name}" }
                            span { class: "cat-quiet text-xs", "{user_usage(user, limit)}" }
                        }
                    }
                }
            }
            // A paired server's users, as one row each server (P8-07).
            for line in usage.pairs.iter().map(|p| pair_usage(p, usage.daily_limit_per_peer)) {
                p { key: "{line}", class: "m-0 text-sm", "{line}" }
            }
        }
    }
}

/// How the saved export is going: the WebTelemetry board's line, or one
/// of WebTelemetryStates' boxes.
#[component]
fn ExportLine(state: ExportState, on_try: EventHandler<()>) -> Element {
    let applies = rsx! {
        span { class: "state state-ok inline-flex items-center gap-1.5", icons::CircleCheck { size: 14 } "Applies immediately" }
    };
    match state {
        ExportState::Off => rsx! {
            div { class: "tele-status",
                span { class: "text-[13px] text-ctp-subtext0", "Nothing is exported." }
                span { class: "grow" }
                {applies}
            }
        },
        ExportState::Starting => rsx! {
            div { class: "tele-status",
                span { class: "text-[13px] text-ctp-subtext0", "Starting · nothing exported yet" }
                span { class: "grow" }
                {applies}
            }
        },
        ExportState::Exporting { last_at, dropped } => rsx! {
            div { class: "tele-status",
                span { class: "state state-ok inline-flex items-center gap-1.5 text-[13px]",
                    icons::CircleCheck { size: 15 }
                    "{exporting_line(now_ms(), last_at, dropped)}"
                }
                span { class: "grow" }
                {applies}
            }
        },
        ExportState::Rejected { error } => rsx! {
            div { class: "alert alert-error", role: "alert",
                icons::CircleX {}
                span { class: "flex flex-col gap-[3px]",
                    b { "The collector refused the data" }
                    span { class: "text-ctp-text", "{error}. Check the headers it needs, such as Authorization." }
                }
            }
            div { class: "flex items-center gap-2",
                button { class: "btn h-[30px]", onclick: move |_| on_try.call(()), icons::RotateCcw { size: 15 } "Try now" }
                span { class: "grow" }
                {applies}
            }
        },
        ExportState::Failing {
            host,
            unreachable,
            error,
            dropped,
            since,
            every_secs,
        } => {
            let since = since.map(local_hours_minutes);
            rsx! {
                div { class: "alert alert-warning", role: "alert",
                    icons::Warning {}
                    span { class: "flex flex-col gap-[3px]",
                        b {
                            if unreachable { "Can't reach {host}" } else { "Exports to {host} fail" }
                        }
                        span { class: "text-ctp-text",
                            "{error}. Exports are batched and dropped when the queue is full, so the server isn't slowed. {dropping_line(dropped, since.as_deref(), every_secs)}"
                        }
                    }
                }
                div { class: "flex items-center gap-2",
                    button { class: "btn h-[30px]", onclick: move |_| on_try.call(()), icons::RotateCcw { size: 15 } "Try now" }
                    span { class: "grow" }
                    {applies}
                }
            }
        }
    }
}
