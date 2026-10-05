//! Settings › Telemetry (P3-14), as the WebTelemetry and
//! WebTelemetryStates boards lay it out: Prometheus's `/metrics` and who
//! may read it, and the OTLP export with how it is going. Both are off
//! until an admin turns them on, and both apply as soon as they are
//! saved. The export's header values are write-only: the tab shows their
//! names, and can replace them all or take them out, never read them.

use dioxus::prelude::*;
use serde_json::{Value, json};
use spoolframe_admin_api::{AdminApiClient, Settings, TelemetryStatus};
use wasm_bindgen::JsValue;

use super::admin::sleep;
use super::wizard::{Footer, Mode, Note, entry, load_settings, loading, locked, save, text};
use super::{Alert, icons, now_ms, page_address, rpc};
use crate::logic::ApiError;
use crate::logs::hours_minutes;
use crate::telemetry::{
    ExportState, HeaderRow, add_address, allowed_from, dropping_line, export_state, exporting_line,
    headers_value, metrics_from_value, refused_line, refused_to_offer, remove_address,
};

/// How often the export's status is asked for while the tab is open.
const STATUS_MS: i32 = 5_000;

async fn load_status() -> Result<TelemetryStatus, ApiError> {
    let client = rpc::client().await?;
    client.telemetry_status().await.map_err(rpc::error)
}

/// What becomes of the headers at Save.
#[derive(Debug, Clone, PartialEq)]
enum Headers {
    /// As they are.
    Keep,
    /// Every one typed again: their values cannot be read back.
    Replace(Vec<HeaderRow>),
    /// Taken out.
    Clear,
}

/// A setting's value at Save: `None`, taking out the runtime value, when
/// nothing sets it and `value` is its default, so the runtime settings
/// hold only what was changed here.
fn unless_default(settings: &Settings, key: &str, value: Value, default: Value) -> Option<Value> {
    let current = entry(settings, key).and_then(|e| e.value.clone());
    if current.is_none() && value == default {
        None
    } else {
        Some(value)
    }
}

/// A boolean setting's value, or its default when nothing sets it.
fn flag_or(settings: &Settings, key: &str, default: bool) -> bool {
    entry(settings, key)
        .and_then(|e| e.value.as_ref())
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// The browser's local `HH:MM` of Unix milliseconds `at`.
fn local_hours_minutes(at: i64) -> String {
    let offset_min = js_sys::Date::new(&JsValue::from_f64(at as f64)).get_timezone_offset();
    hours_minutes(at - (offset_min * 60_000.0) as i64)
}

#[component]
pub fn TelemetryForm(on_done: EventHandler<()>) -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let mut status = use_signal(|| None::<Result<TelemetryStatus, ApiError>>);
    use_hook(move || {
        spawn(async move {
            loop {
                status.set(Some(load_status().await));
                sleep(STATUS_MS).await;
            }
        });
    });
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    rsx! {
        for g in [generation()] {
            TelemetryFields {
                key: "{g}",
                settings: loaded.clone(),
                status: status(),
                on_done,
                on_reload: move |_| generation += 1,
                on_try: move |_| {
                    spawn(async move { status.set(Some(load_status().await)) });
                },
            }
        }
    }
}

#[component]
fn TelemetryFields(
    settings: Settings,
    status: Option<Result<TelemetryStatus, ApiError>>,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
    on_try: EventHandler<()>,
) -> Element {
    let s = &settings;
    let mut metrics_on = use_signal(|| flag_or(s, "admin.metrics", false));
    let from_setting = text(s, "admin.metrics_from");
    let mut from =
        use_signal(|| allowed_from((!from_setting.is_empty()).then_some(from_setting.as_str())));
    let mut adding = use_signal(|| None::<String>);
    let mut add_error = use_signal(|| None::<String>);
    let endpoint_now = text(s, "telemetry.otlp.endpoint");
    let mut otlp_on = use_signal(|| !endpoint_now.trim().is_empty());
    let endpoint = use_signal(|| endpoint_now.clone());
    let mut protocol = use_signal(|| match text(s, "telemetry.otlp.protocol").as_str() {
        "http" => "http".to_owned(),
        _ => "grpc".to_owned(),
    });
    let traces = use_signal(|| flag_or(s, "telemetry.otlp.traces", true));
    let metrics = use_signal(|| flag_or(s, "telemetry.otlp.metrics", true));
    let logs = use_signal(|| flag_or(s, "telemetry.otlp.logs", true));
    let mut headers = use_signal(|| Headers::Keep);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);

    let headers_were_set = entry(s, "telemetry.otlp.headers").is_some_and(|e| e.set);
    let saved = settings.clone();
    let on_save = move |_| {
        let s = saved.clone();
        let endpoint_typed = endpoint().trim().to_owned();
        if otlp_on() && endpoint_typed.is_empty() {
            error.set(Some(
                "Type the collector's address, such as http://alloy.home.arpa:4317.".into(),
            ));
            return;
        }
        let header_change = match headers() {
            Headers::Keep => None,
            Headers::Clear => Some(None),
            Headers::Replace(rows) => match headers_value(&rows) {
                // None typed where none were set: nothing to take out.
                Ok(None) if !headers_were_set => None,
                Ok(value) => Some(value.map(Value::String)),
                Err(e) => {
                    error.set(Some(e));
                    return;
                }
            },
        };
        let metrics_value = if metrics_on() {
            Some(json!(true))
        } else {
            unless_default(&s, "admin.metrics", json!(false), json!(false))
        };
        let mut changes = vec![
            ("admin.metrics", metrics_value),
            (
                "admin.metrics_from",
                metrics_from_value(&from()).map(Value::String),
            ),
        ];
        if otlp_on() {
            changes.extend([
                ("telemetry.otlp.endpoint", Some(json!(endpoint_typed))),
                (
                    "telemetry.otlp.protocol",
                    unless_default(
                        &s,
                        "telemetry.otlp.protocol",
                        json!(protocol()),
                        json!("grpc"),
                    ),
                ),
                (
                    "telemetry.otlp.traces",
                    unless_default(&s, "telemetry.otlp.traces", json!(traces()), json!(true)),
                ),
                (
                    "telemetry.otlp.metrics",
                    unless_default(&s, "telemetry.otlp.metrics", json!(metrics()), json!(true)),
                ),
                (
                    "telemetry.otlp.logs",
                    unless_default(&s, "telemetry.otlp.logs", json!(logs()), json!(true)),
                ),
            ]);
        } else {
            // Off is no endpoint; the rest stays for next time.
            changes.push(("telemetry.otlp.endpoint", None));
        }
        busy.set(true);
        spawn(async move {
            let done = async {
                save(&s, changes).await?;
                if let Some(value) = header_change {
                    let client = rpc::client().await?;
                    match value {
                        Some(value) => {
                            client
                                .setting_set("telemetry.otlp.headers".into(), value)
                                .await
                                .map_err(rpc::error)?;
                        }
                        None => {
                            client
                                .setting_unset("telemetry.otlp.headers".into())
                                .await
                                .map_err(rpc::error)?;
                        }
                    }
                }
                Ok::<(), ApiError>(())
            };
            match done.await {
                Ok(()) => on_done.call(()),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };

    let (status, status_error) = match status {
        Some(Ok(t)) => (Some(t), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    let path = status
        .as_ref()
        .map(|t| t.metrics.path.clone())
        .unwrap_or_else(|| "/metrics".into());
    let refused: Vec<_> = status
        .as_ref()
        .map(|t| {
            refused_to_offer(&t.metrics.refused, &from())
                .into_iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let otlp = status.as_ref().and_then(|t| t.otlp.clone());
    let state = export_state(otlp.as_ref());
    let header_names = otlp.as_ref().map(|o| o.header_names.clone());
    let headers_set = headers_were_set;
    let resource = status
        .as_ref()
        .map(|t| t.resource.clone())
        .unwrap_or_default();
    let metrics_locked = locked(s, "admin.metrics");
    let from_locked = locked(s, "admin.metrics_from");
    let endpoint_locked = locked(s, "telemetry.otlp.endpoint");
    let headers_locked = locked(s, "telemetry.otlp.headers");
    let mut add = move || {
        let typed = adding().unwrap_or_default();
        match add_address(&from(), &typed) {
            Ok(list) => {
                from.set(list);
                adding.set(None);
                add_error.set(None);
            }
            Err(e) => add_error.set(Some(e)),
        }
    };

    rsx! {
        if let Some(e) = status_error {
            Alert { text: format!("Couldn't read how the exports are going: {}", e.explain()) }
        }
        section { class: "card tele-card",
            h2 { "Prometheus" }
            div { class: "tele-row",
                button {
                    class: "switch mt-0.5",
                    role: "switch",
                    aria_checked: "{metrics_on()}",
                    aria_label: "Serve /metrics",
                    disabled: metrics_locked,
                    onclick: move |_| metrics_on.toggle(),
                }
                span { class: "choice-what",
                    b { "Serve {path}" }
                    if metrics_on() {
                        span {
                            "On the admin listener: "
                            span { class: "mono-value text-xs", "{page_address()}{path}" }
                            ". Off by default."
                        }
                    } else {
                        "Off: the address answers “not found”."
                    }
                }
            }
            Note { settings: settings.clone(), setting: "admin.metrics", on_changed: move |_| on_reload.call(()) }
            if metrics_on() {
                div { class: "tele-row flex-col",
                    b { class: "text-sm", "Allowed from" }
                    div { class: "flex flex-wrap items-center gap-1.5",
                        for e in from() {
                            span { key: "{e}", class: "tele-chip",
                                "{e}"
                                if !crate::telemetry::is_loopback(&e) && !from_locked {
                                    button {
                                        aria_label: "Remove {e}",
                                        onclick: {
                                            let e = e.clone();
                                            move |_| from.set(remove_address(&from(), &e))
                                        },
                                        icons::X {}
                                    }
                                }
                            }
                        }
                        if adding().is_none() && !from_locked {
                            button { class: "btn h-[30px]", onclick: move |_| adding.set(Some(String::new())),
                                icons::Plus { size: 15 }
                                "Add an address"
                            }
                        }
                    }
                    if let Some(typed) = adding() {
                        form {
                            class: "flex flex-wrap items-center gap-2",
                            onsubmit: move |e| {
                                e.prevent_default();
                                add();
                            },
                            input {
                                class: "tele-input w-[220px]",
                                aria_label: "Address or range",
                                placeholder: "10.10.0.2 or 10.10.0.0/24",
                                value: "{typed}",
                                oninput: move |e| adding.set(Some(e.value())),
                            }
                            button { class: "btn btn-teal h-[34px]", r#type: "submit", "Add" }
                            button {
                                class: "btn h-[34px]",
                                r#type: "button",
                                onclick: move |_| {
                                    adding.set(None);
                                    add_error.set(None);
                                },
                                "Cancel"
                            }
                        }
                    }
                    if let Some(e) = add_error() {
                        span { class: "field-error", "{e}" }
                    }
                    span { class: "text-xs text-ctp-subtext0",
                        "Anyone else gets refused. Loopback is always allowed, so a scraper on this machine works."
                    }
                    Note { settings: settings.clone(), setting: "admin.metrics_from", on_changed: move |_| on_reload.call(()) }
                }
                for r in refused {
                    div { key: "{r.address}", class: "tele-row flex-col",
                        span { class: "state state-warn inline-flex items-center gap-1.5 text-[13px]",
                            icons::Warning {}
                            "{refused_line(&r)}"
                        }
                        span { class: "text-[13px] text-ctp-subtext0", "Add the address under Allowed from if that's your Prometheus." }
                        if !from_locked {
                            div {
                                button {
                                    class: "btn h-[30px]",
                                    onclick: {
                                        let address = r.address.clone();
                                        move |_| {
                                            if let Ok(list) = add_address(&from(), &address) {
                                                from.set(list);
                                            }
                                        }
                                    },
                                    icons::Plus { size: 15 }
                                    "Allow {r.address}"
                                }
                            }
                        }
                    }
                }
            }
        }
        section { class: "card tele-card",
            h2 { "OpenTelemetry" }
            div { class: "tele-row",
                button {
                    class: "switch mt-0.5",
                    role: "switch",
                    aria_checked: "{otlp_on()}",
                    aria_label: "Export with OTLP",
                    disabled: endpoint_locked,
                    onclick: move |_| otlp_on.toggle(),
                }
                span { class: "choice-what",
                    b { "Export with OTLP" }
                    "Off by default: with no endpoint, nothing leaves the server."
                }
            }
            if otlp_on() {
                div { class: "tele-row flex-col",
                    super::Field { label: "Endpoint", value: endpoint, placeholder: "http://alloy.home.arpa:4317", disabled: endpoint_locked }
                    span { class: "text-xs text-ctp-subtext0", "An OpenTelemetry Collector or Grafana Alloy, for example." }
                    Note { settings: settings.clone(), setting: "telemetry.otlp.endpoint", on_changed: move |_| on_reload.call(()) }
                }
                div { class: "tele-row flex-col",
                    b { class: "text-sm", "Protocol" }
                    div { class: "seg", role: "radiogroup", aria_label: "Protocol",
                        for (value, label) in [("grpc", "gRPC"), ("http", "HTTP")] {
                            button {
                                key: "{value}",
                                class: if protocol() == value { "seg-item seg-item-on" } else { "seg-item" },
                                role: "radio",
                                aria_checked: "{protocol() == value}",
                                disabled: locked(s, "telemetry.otlp.protocol"),
                                onclick: move |_| protocol.set(value.to_owned()),
                                "{label}"
                            }
                        }
                    }
                }
                div { class: "tele-row flex-col",
                    b { class: "text-sm", "Headers" }
                    match headers() {
                        Headers::Replace(rows) => rsx! {
                            for (i, row) in rows.iter().cloned().enumerate() {
                                div { key: "{i}", class: "tele-header",
                                    input {
                                        class: "tele-input",
                                        aria_label: "Header name",
                                        placeholder: "Authorization",
                                        value: "{row.name}",
                                        oninput: move |e| headers.with_mut(|h| if let Headers::Replace(rows) = h { rows[i].name = e.value() }),
                                    }
                                    input {
                                        class: "tele-input",
                                        r#type: "password",
                                        autocomplete: "off",
                                        aria_label: "Header value",
                                        placeholder: "Value",
                                        value: "{row.value}",
                                        oninput: move |e| headers.with_mut(|h| if let Headers::Replace(rows) = h { rows[i].value = e.value() }),
                                    }
                                    button {
                                        class: "text-ctp-subtext0",
                                        aria_label: "Remove header",
                                        onclick: move |_| headers.with_mut(|h| if let Headers::Replace(rows) = h { rows.remove(i); }),
                                        icons::Trash {}
                                    }
                                }
                            }
                            div { class: "flex flex-wrap gap-2",
                                button {
                                    class: "btn h-[30px]",
                                    onclick: move |_| headers.with_mut(|h| if let Headers::Replace(rows) = h { rows.push(HeaderRow::default()) }),
                                    icons::Plus { size: 15 }
                                    "Add a header"
                                }
                                if headers_set {
                                    button { class: "btn h-[30px]", onclick: move |_| headers.set(Headers::Keep), "Keep the headers as they are" }
                                }
                            }
                            if headers_set {
                                span { class: "text-xs text-ctp-subtext0", "Saving replaces every header: type each one's value again." }
                            }
                        },
                        Headers::Clear => rsx! {
                            span { class: "text-sm text-ctp-subtext0", "No headers are sent once saved." }
                            div {
                                button { class: "btn h-[30px]", onclick: move |_| headers.set(Headers::Keep), "Keep the headers as they are" }
                            }
                        },
                        Headers::Keep if headers_set => rsx! {
                            match &header_names {
                                Some(names) if !names.is_empty() => rsx! {
                                    for name in names.iter().cloned() {
                                        div { key: "{name}", class: "tele-header",
                                            input { class: "tele-input", aria_label: "Header name", value: "{name}", readonly: true }
                                            input { class: "tele-input", aria_label: "Header value, hidden", value: "••••••••••••••••", readonly: true }
                                            span {}
                                        }
                                    }
                                },
                                _ => rsx! {
                                    span { class: "text-sm text-ctp-subtext0", "Headers are set. Their names show here while the export is on." }
                                },
                            }
                            if !headers_locked {
                                div { class: "flex flex-wrap gap-2",
                                    button {
                                        class: "btn h-[30px]",
                                        onclick: move |_| headers.set(Headers::Replace(vec![HeaderRow::default()])),
                                        icons::Pencil {}
                                        "Replace the headers"
                                    }
                                    button { class: "btn btn-danger h-[30px]", onclick: move |_| headers.set(Headers::Clear),
                                        icons::Trash {}
                                        "Remove them"
                                    }
                                }
                            }
                        },
                        Headers::Keep => rsx! {
                            div {
                                button {
                                    class: "btn h-[30px]",
                                    disabled: headers_locked,
                                    onclick: move |_| headers.set(Headers::Replace(vec![HeaderRow::default()])),
                                    icons::Plus { size: 15 }
                                    "Add a header"
                                }
                            }
                        },
                    }
                    span { class: "text-xs text-ctp-subtext0", "Values are kept like passwords and never shown again." }
                    Note { settings: settings.clone(), setting: "telemetry.otlp.headers", on_changed: move |_| on_reload.call(()) }
                }
                div { class: "tele-row flex-col",
                    b { class: "text-sm", "Send" }
                    div { class: "flex flex-wrap gap-[18px]",
                        for (label, mut on, key) in [
                            ("Traces", traces, "telemetry.otlp.traces"),
                            ("Metrics", metrics, "telemetry.otlp.metrics"),
                            ("Logs", logs, "telemetry.otlp.logs"),
                        ] {
                            label { key: "{label}", class: "inline-flex items-center gap-2 text-sm",
                                button {
                                    r#type: "button",
                                    class: if on() { "box box-on" } else { "box" },
                                    role: "checkbox",
                                    aria_checked: "{on()}",
                                    aria_label: "Send {label}",
                                    disabled: locked(s, key),
                                    onclick: move |_| on.toggle(),
                                    if on() { icons::Check {} }
                                }
                                "{label}"
                            }
                        }
                    }
                }
                if !resource.is_empty() {
                    div { class: "tele-row flex-col",
                        b { class: "text-sm", "Sent with every signal" }
                        div { class: "tele-attrs",
                            for a in resource {
                                span { class: "text-ctp-subtext0", "{a.key}" }
                                span { "{a.value}" }
                            }
                        }
                        span { class: "text-xs text-ctp-subtext0", "Never secrets or tokens; users only as their opaque local:<uuid>." }
                    }
                }
            }
            ExportLine { state, on_try }
        }
        if let Some(text) = error() {
            Alert { text }
        }
        Footer { mode: Mode::Settings, step: "telemetry", busy: busy(), on_save, on_discard: move |_| on_reload.call(()) }
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
