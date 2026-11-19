//! The settings too particular for the generic fields: each still writes
//! to its area's draft, and is saved with the page's Save. Two act at
//! once, as flows of their own: a sign-in provider's dialog (a pasted
//! client secret is kept by its own call), and signing in with Plex
//! (plex.tv hands the server the token).

use dioxus::prelude::*;
use serde_json::{Value, json};
use spoolframe_admin_api::{AdminApiClient, ServerStatus};

use super::settings_area::AreaState;
use super::{Route, icons, rpc};
use crate::network::{Pkarr, pkarr_form, pkarr_setting};
use crate::remote::{Ip, Opened, WanForm, add_static};
use crate::setup;
use crate::telemetry::{
    HeaderRow, add_address, allowed_from, headers_value, is_loopback, metrics_from_value,
    refused_line, refused_to_offer, remove_address,
};
use spoolframe_admin_api::{CustomField as Custom, FieldWidget, SchemaField};

/// A field of its own, by the component its schema names.
#[component]
pub(super) fn CustomField(field: SchemaField) -> Element {
    let setting = crate::settings_layout::key(&field.key);
    match field.custom.unwrap_or(Custom::Unknown) {
        Custom::Wan => rsx! { WanField { setting } },
        Custom::WanSwitch => rsx! { WanSwitch { setting } },
        Custom::Pkarr => rsx! { PkarrField { setting } },
        Custom::Budget => rsx! { BudgetField { field } },
        Custom::Gpus => rsx! { GpusField { setting } },
        Custom::VideoCodecs => rsx! { CodecsField { setting } },
        Custom::Oidc => rsx! {
            // Each provider is saved from its own dialog.
            super::sso::SsoForm { on_done: move |_| {} }
        },
        Custom::Redirects => rsx! { RedirectsField { setting } },
        Custom::Headers => rsx! { HeadersField { setting } },
        Custom::PlexToken => rsx! { PlexTokenField { field } },
        Custom::MetricsFrom => rsx! { MetricsFromField { setting } },
        Custom::Libraries | Custom::PathMapping => rsx! {
            super::settings_area::GuardedLink { to: Route::Libraries {}, "On the Libraries page" }
        },
        // A field a newer server added: edited where that server's own
        // web UI has it.
        Custom::Unknown => rsx! {
            span { class: "text-sm text-ctp-subtext0", "This web UI cannot change it." }
        },
    }
}

async fn status() -> Option<ServerStatus> {
    let client = rpc::client().await.ok()?;
    client.server_status().await.ok()
}

/// `network.wan`'s mapped port and public addresses. Whether it is
/// advertised at all is the Sensitive block's switch ([`WanSwitch`]):
/// until it is on, these wait.
#[component]
fn WanField(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let start = WanForm::from_setting(state.shown(key).as_ref());
    let on = state.shown(key).is_some_and(|v| v.is_object());
    let fixed = state.locked(key) || !on;
    let mut mapped = use_signal(|| start.mapped);
    let mut port = use_signal(|| start.port.clone());
    let mut opened = use_signal(|| start.opened);
    let mut ip = use_signal(|| start.ip);
    let mut typed_ip = use_signal(|| start.typed_ip.clone());
    let mut lookup_service = use_signal(|| start.lookup_service);
    let mut statics = use_signal(|| start.statics.clone());
    let mut new_static = use_signal(String::new);
    let commit = move || {
        let form = WanForm {
            on: true,
            mapped: *mapped.peek(),
            port: port.peek().clone(),
            opened: *opened.peek(),
            ip: *ip.peek(),
            typed_ip: typed_ip.peek().clone(),
            lookup_service: *lookup_service.peek(),
            statics: statics.peek().clone(),
        };
        state.put(key, form.to_setting());
    };
    let seg = |here: bool| {
        if here {
            "seg-item seg-item-on"
        } else {
            "seg-item"
        }
    };
    rsx! {
        div { class: "setting-control flex flex-col gap-3",
            super::remote::RemoteStatus { opened: start.opened }
            if !on {
                span { class: "text-xs text-ctp-subtext0", "Turn on “Advertise a public address” under Sensitive first." }
            }
            label { class: "flex items-center gap-2.5 text-sm",
                button {
                    class: "switch",
                    role: "switch",
                    r#type: "button",
                    aria_checked: "{mapped()}",
                    aria_label: "Mapped port",
                    disabled: fixed,
                    onclick: move |_| {
                        mapped.toggle();
                        commit();
                    },
                }
                "A port on your router"
            }
            if mapped() {
                div { class: "seg", role: "radiogroup", aria_label: "How the port is opened",
                    for (value, name) in [(Opened::Automatic, "Automatic"), (Opened::Forwarded, "I forwarded it myself")] {
                        button {
                            key: "{name}",
                            class: seg(opened() == value),
                            r#type: "button",
                            role: "radio",
                            aria_checked: "{opened() == value}",
                            disabled: fixed,
                            onclick: move |_| {
                                opened.set(value);
                                commit();
                            },
                            "{name}"
                        }
                    }
                }
                if opened() == Opened::Forwarded {
                    input {
                        class: "setting-input w-32",
                        aria_label: "Public port",
                        inputmode: "numeric",
                        placeholder: "Port",
                        value: "{port}",
                        disabled: fixed,
                        oninput: move |e| {
                            port.set(e.value());
                            commit();
                        },
                    }
                }
                div { class: "seg", role: "radiogroup", aria_label: "Public IP",
                    for (value, name) in [(Ip::LookUp, "Look it up"), (Ip::Typed, "This address")] {
                        button {
                            key: "{name}",
                            class: seg(ip() == value),
                            r#type: "button",
                            role: "radio",
                            aria_checked: "{ip() == value}",
                            disabled: fixed,
                            onclick: move |_| {
                                ip.set(value);
                                commit();
                            },
                            "{name}"
                        }
                    }
                }
                if ip() == Ip::Typed {
                    input {
                        class: "setting-input",
                        aria_label: "Public IP address",
                        placeholder: "203.0.113.24",
                        value: "{typed_ip}",
                        disabled: fixed,
                        oninput: move |e| {
                            typed_ip.set(e.value());
                            commit();
                        },
                    }
                }
                div { class: "seg", role: "radiogroup", aria_label: "Lookup service",
                    for (value, name) in [(true, "wtfismyip.com"), (false, "None")] {
                        button {
                            key: "{name}",
                            class: seg(lookup_service() == value),
                            r#type: "button",
                            role: "radio",
                            aria_checked: "{lookup_service() == value}",
                            disabled: fixed,
                            onclick: move |_| {
                                lookup_service.set(value);
                                commit();
                            },
                            "{name}"
                        }
                    }
                }
            }
            b { class: "text-sm", "Static addresses" }
            for (i, address) in statics().into_iter().enumerate() {
                div { key: "{address}", class: "flex items-center gap-2",
                    span { class: "mono-value flex-grow text-sm", "{address}" }
                    button {
                        class: "icon-button",
                        r#type: "button",
                        aria_label: "Remove {address}",
                        disabled: fixed,
                        onclick: move |_| {
                            statics.write().remove(i);
                            commit();
                        },
                        icons::Trash {}
                    }
                }
            }
            form {
                class: "flex items-center gap-2",
                onsubmit: move |e: FormEvent| {
                    e.prevent_default();
                    let added = add_static(&new_static.peek(), &statics.peek());
                    if let Ok(address) = added {
                        statics.write().push(address);
                        new_static.set(String::new());
                        commit();
                    }
                },
                input {
                    class: "setting-input",
                    aria_label: "Add a public address",
                    placeholder: "203.0.113.7:7420",
                    value: "{new_static}",
                    disabled: fixed,
                    oninput: move |e| new_static.set(e.value()),
                }
                button {
                    class: "btn-dialog btn-dialog-grey",
                    r#type: "submit",
                    disabled: fixed || add_static(&new_static(), &statics()).is_err(),
                    icons::Plus {}
                    "Add"
                }
            }
        }
    }
}

/// "Advertise a public address": `network.wan` there or not.
#[component]
fn WanSwitch(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let on = state.shown(key).is_some_and(|v| v.is_object());
    rsx! {
        div { class: "setting-control",
            button {
                class: "switch",
                role: "switch",
                r#type: "button",
                aria_checked: "{on}",
                aria_label: "Advertise a public address",
                disabled: state.locked(key),
                onclick: move |_| {
                    if on {
                        state.put(key, Ok(None));
                    } else {
                        let saved = state.entry(key).and_then(|e| e.value);
                        let form = WanForm {
                            on: true,
                            ..WanForm::from_setting(saved.as_ref())
                        };
                        state.put(key, form.to_setting());
                    }
                },
            }
        }
    }
}

#[component]
fn PkarrField(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let (start, start_url) = pkarr_form(state.shown(key).as_ref());
    let mut choice = use_signal(|| start);
    let mut url = use_signal(|| start_url);
    let locked = state.locked(key);
    let commit = move || {
        state.put(key, pkarr_setting(*choice.peek(), &url.peek()).map(Some));
    };
    rsx! {
        div { class: "setting-control flex flex-col gap-2",
            div { class: "seg", role: "radiogroup", aria_label: "Find the server by its key",
                for (value, name) in [(Pkarr::Off, "Off"), (Pkarr::N0, "n0 (dns.iroh.link)"), (Pkarr::Own, "My own pkarr relay")] {
                    button {
                        key: "{name}",
                        class: if choice() == value { "seg-item seg-item-on" } else { "seg-item" },
                        r#type: "button",
                        role: "radio",
                        aria_checked: "{choice() == value}",
                        disabled: locked,
                        onclick: move |_| {
                            choice.set(value);
                            commit();
                        },
                        "{name}"
                    }
                }
            }
            if choice() == Pkarr::Own {
                input {
                    class: "setting-input",
                    aria_label: "Your pkarr relay",
                    placeholder: "https://dns.example.org/pkarr",
                    value: "{url}",
                    disabled: locked,
                    oninput: move |e| {
                        url.set(e.value());
                        commit();
                    },
                }
            }
        }
    }
}

/// The upload budget, and measuring it.
#[component]
fn BudgetField(field: SchemaField) -> Element {
    let key = crate::settings_layout::key(&field.key);
    let state = use_context::<AreaState>();
    let reads = field.clone();
    let mut text =
        use_signal(|| crate::settings_layout::text_of(&field, state.shown(key).as_ref()));
    let mut measured = use_signal(|| None::<spoolframe_admin_api::UplinkMeasurement>);
    let mut measuring = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let locked = state.locked(key);
    let set = use_callback(move |typed: String| {
        text.set(typed.clone());
        state.put(key, crate::settings_layout::read(&reads, &typed));
    });
    let measure = move |_| {
        measuring.set(true);
        error.set(None);
        spawn(async move {
            let found = async {
                let client = rpc::client().await?;
                client.uplink_measure().await.map_err(rpc::error)
            };
            match found.await {
                Ok(m) => measured.set(Some(m)),
                Err(e) => error.set(Some(e.explain())),
            }
            measuring.set(false);
        });
    };
    rsx! {
        div { class: "setting-control flex flex-col gap-2",
            div { class: "flex items-center gap-2",
                input {
                    class: "setting-input w-32",
                    aria_label: "Upload budget",
                    inputmode: "numeric",
                    value: "{text}",
                    disabled: locked,
                    oninput: move |e| set.call(e.value()),
                }
                span { class: "text-sm text-ctp-subtext0", "Mbit/s" }
                button {
                    class: "btn-dialog btn-dialog-grey",
                    r#type: "button",
                    disabled: measuring() || locked,
                    onclick: measure,
                    if measuring() { "Measuring… (about 30 MB to Cloudflare)" } else { "Measure the upload" }
                }
            }
            span { class: "text-[13px] text-ctp-subtext0", "{crate::uplink::budget_meaning(&text())}" }
            if let Some(m) = measured() {
                div { class: "flex flex-wrap items-center gap-2",
                    span { class: "text-[13px]", "{crate::uplink::measured_line(&m)}" }
                    button {
                        class: "btn-dialog btn-dialog-primary",
                        r#type: "button",
                        onclick: move |_| set.call(m.suggested_budget_mbps.to_string()),
                        "{crate::uplink::use_suggested(&m)}"
                    }
                }
            }
            if let Some(why) = error() {
                span { class: "text-xs text-ctp-red", "{why}" }
            }
        }
    }
}

/// `transcode.gpus`: which GPUs, in order, or none.
#[component]
fn GpusField(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let start = state.shown(key);
    let mut rows = use_signal(|| setup::GpuRow::rows(start.as_ref()));
    let mut none = use_signal(|| matches!(&start, Some(Value::Array(list)) if list.is_empty()));
    let locked = state.locked(key);
    let commit = move || {
        let wanted = match (*none.peek(), rows.peek().is_empty()) {
            (true, _) => Ok(Some(json!([]))),
            (false, true) => Ok(None),
            (false, false) => setup::GpuRow::setting(&rows.peek()).map(Some),
        };
        state.put(key, wanted);
    };
    let off = locked || none();
    rsx! {
        div { class: "setting-control flex flex-col gap-2",
            for (i, row) in rows().into_iter().enumerate() {
                div { key: "{i}", class: "flex flex-wrap items-center gap-2",
                    span { class: "text-xs text-ctp-subtext0", "{i + 1}." }
                    select {
                        class: "pref-select",
                        aria_label: "GPU {i + 1}: API",
                        disabled: off,
                        onchange: move |e| {
                            rows.write()[i].api = e.value();
                            commit();
                        },
                        for (api, label) in setup::GPU_APIS {
                            option { key: "{api}", value: "{api}", selected: row.api == api, "{label}" }
                        }
                    }
                    select {
                        class: "pref-select",
                        aria_label: "GPU {i + 1}: which cards",
                        disabled: off,
                        onchange: move |e| {
                            rows.write()[i].pick = e.value();
                            commit();
                        },
                        for (pick, label) in setup::GPU_PICKS {
                            option { key: "{pick}", value: "{pick}", selected: row.pick == pick, "{label}" }
                        }
                    }
                    if row.pick == "device" {
                        input {
                            class: "setting-input",
                            value: "{row.device}",
                            placeholder: if row.api == "nvenc" { "CUDA index: 0" } else { "/dev/dri/by-path/pci-…-render" },
                            aria_label: "GPU {i + 1}: the card",
                            disabled: off,
                            oninput: move |e| {
                                rows.write()[i].device = e.value();
                                commit();
                            },
                        }
                    }
                    input {
                        class: "setting-input w-32",
                        value: "{row.max_jobs}",
                        placeholder: "Jobs at once",
                        inputmode: "numeric",
                        aria_label: "GPU {i + 1}: jobs at once on each card",
                        disabled: off,
                        oninput: move |e| {
                            rows.write()[i].max_jobs = e.value();
                            commit();
                        },
                    }
                    button {
                        class: "icon-button",
                        r#type: "button",
                        aria_label: "Remove GPU {i + 1}",
                        disabled: off,
                        onclick: move |_| {
                            rows.write().remove(i);
                            commit();
                        },
                        icons::Trash {}
                    }
                }
            }
            button {
                class: "btn-dialog btn-dialog-grey self-start",
                r#type: "button",
                disabled: off,
                onclick: move |_| {
                    rows.write().push(setup::GpuRow::new());
                    commit();
                },
                icons::Plus {}
                "Add GPUs"
            }
            label { class: "flex items-center gap-2.5 text-sm",
                button {
                    class: "switch",
                    role: "switch",
                    r#type: "button",
                    aria_checked: "{none()}",
                    aria_label: "No GPU: software only",
                    disabled: locked,
                    onclick: move |_| {
                        none.toggle();
                        commit();
                    },
                }
                "No GPU: every encode in software, on the CPU"
            }
        }
    }
}

/// `transcode.video_codecs`, in order, with which GPU encodes each.
#[component]
fn CodecsField(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let rows = use_signal(|| setup::codec_rows(state.shown(key).as_ref()));
    let found = use_resource(status);
    let encoders = found
        .read()
        .clone()
        .flatten()
        .map(|s| s.transcoding.video_encoders)
        .unwrap_or_default();
    let gpu_encodes = setup::gpu_encoders_by_codec(&encoders);
    // The order and the boxes are the order component's own: their value
    // goes to the draft as they change. An unchanged order is no edit.
    use_effect(move || {
        let value = setup::codecs_setting(&rows.read());
        state.put(key, Ok(Some(value)));
    });
    rsx! {
        div { class: "setting-control flex flex-col gap-2",
            p { class: "m-0 text-xs text-ctp-subtext0", {setup::VIDEO_CODECS_NOTE} }
            super::codecs::CodecOrder { rows, locked: state.locked(key), gpu_encodes }
        }
    }
}

/// `mcp.allowed_redirects`: the apps allowed to sign in.
#[component]
fn RedirectsField(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let mut list = use_signal(|| crate::connect::allowed_redirects(state.shown(key).as_ref()));
    let mut name = use_signal(String::new);
    let mut pattern = use_signal(String::new);
    let mut refusal = use_signal(|| None::<String>);
    let locked = state.locked(key);
    let commit = move || {
        state.put(key, Ok(Some(crate::connect::redirects_value(&list.peek()))));
    };
    let missing = !crate::connect::missing_defaults(&list()).is_empty();
    // As saved: an agent added before MCP is on would find no server.
    let mcp_on = state
        .entry("mcp.enabled")
        .and_then(|e| e.value)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    rsx! {
        if mcp_on {
            super::remote::McpHowTo {}
        }
        div { class: "setting-control flex flex-col gap-2",
            for (i, entry) in list().into_iter().enumerate() {
                div { key: "{i}-{entry.pattern}", class: "flex items-center gap-2",
                    span { class: "flex min-w-0 flex-grow flex-col gap-0.5",
                        span { class: "text-sm font-semibold", "{entry.name}" }
                        span { class: "mono-value min-w-0 break-all text-xs", "{entry.pattern}" }
                    }
                    button {
                        class: "icon-button",
                        r#type: "button",
                        aria_label: "Remove {entry.name}",
                        disabled: locked,
                        onclick: move |_| {
                            list.write().remove(i);
                            commit();
                        },
                        icons::Trash {}
                    }
                }
            }
            if list().is_empty() {
                span { class: "text-xs text-ctp-yellow", "No app is allowed: none can connect." }
            }
            if !locked {
                form {
                    class: "flex flex-wrap items-center gap-2",
                    onsubmit: move |e: FormEvent| {
                        e.prevent_default();
                        let added = crate::connect::add_redirect(&list.peek(), &name.peek(), &pattern.peek());
                        match added {
                            Ok(added) => {
                                list.set(added);
                                name.set(String::new());
                                pattern.set(String::new());
                                refusal.set(None);
                                commit();
                            }
                            Err(why) => refusal.set(Some(why)),
                        }
                    },
                    input {
                        class: "setting-input w-40",
                        aria_label: "Name",
                        placeholder: "My agent",
                        value: "{name}",
                        oninput: move |e| name.set(e.value()),
                    }
                    input {
                        class: "setting-input flex-grow",
                        aria_label: "Redirect URI pattern",
                        placeholder: "https://agent.example/oauth/callback",
                        value: "{pattern}",
                        oninput: move |e| pattern.set(e.value()),
                    }
                    button {
                        class: "btn-dialog btn-dialog-grey",
                        r#type: "submit",
                        disabled: name().trim().is_empty() || pattern().trim().is_empty(),
                        icons::Plus {}
                        "Add"
                    }
                }
                if let Some(why) = refusal() {
                    span { class: "text-xs text-ctp-red", "{why}" }
                }
                if missing {
                    button {
                        class: "link-button self-start text-xs",
                        r#type: "button",
                        onclick: move |_| {
                            let restored = crate::connect::restore_defaults(&list.peek());
                            list.set(restored);
                            commit();
                        },
                        "Restore Claude and Codex"
                    }
                }
            }
        }
    }
}

/// What becomes of the OTLP headers at Save.
#[derive(Debug, Clone, PartialEq)]
enum Headers {
    /// As they are: their values cannot be read back.
    Keep,
    /// Every one typed again.
    Replace(Vec<HeaderRow>),
    /// Taken out.
    Clear,
}

/// `telemetry.otlp.headers`: kept like passwords, so replaced whole.
#[component]
fn HeadersField(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let set = state.entry(key).is_some_and(|e| e.set);
    let mut headers = use_signal(|| Headers::Keep);
    let locked = state.locked(key);
    let commit = move || match &*headers.peek() {
        Headers::Keep => state.forget(key),
        Headers::Clear => state.put(key, Ok(None)),
        Headers::Replace(rows) => {
            let wanted = headers_value(rows).map(|t| t.map(Value::Object));
            state.put(key, wanted);
        }
    };
    rsx! {
        div { class: "setting-control flex flex-col gap-2",
            match headers() {
                Headers::Replace(rows) => rsx! {
                    for (i, row) in rows.iter().cloned().enumerate() {
                        div { key: "{i}", class: "flex flex-wrap items-center gap-2",
                            input {
                                class: "setting-input w-44",
                                aria_label: "Header name",
                                placeholder: "Authorization",
                                value: "{row.name}",
                                oninput: move |e| {
                                    headers.with_mut(|h| if let Headers::Replace(rows) = h { rows[i].name = e.value() });
                                    commit();
                                },
                            }
                            form { class: "contents", onsubmit: move |e: FormEvent| e.prevent_default(),
                                input {
                                    class: "setting-input flex-grow",
                                    r#type: "password",
                                    autocomplete: "off",
                                    aria_label: "Header value",
                                    placeholder: "Value",
                                    value: "{row.value}",
                                    oninput: move |e| {
                                        headers.with_mut(|h| if let Headers::Replace(rows) = h { rows[i].value = e.value() });
                                        commit();
                                    },
                                }
                            }
                            button {
                                class: "icon-button",
                                r#type: "button",
                                aria_label: "Remove header",
                                onclick: move |_| {
                                    headers.with_mut(|h| if let Headers::Replace(rows) = h { rows.remove(i); });
                                    commit();
                                },
                                icons::Trash {}
                            }
                        }
                    }
                    div { class: "flex flex-wrap gap-2",
                        button {
                            class: "btn h-[30px]",
                            r#type: "button",
                            onclick: move |_| {
                                headers.with_mut(|h| if let Headers::Replace(rows) = h { rows.push(HeaderRow::default()) });
                                commit();
                            },
                            icons::Plus { size: 15 }
                            "Add a header"
                        }
                        if set {
                            button {
                                class: "btn h-[30px]",
                                r#type: "button",
                                onclick: move |_| {
                                    headers.set(Headers::Keep);
                                    commit();
                                },
                                "Keep the headers as they are"
                            }
                        }
                    }
                    if set {
                        span { class: "text-xs text-ctp-subtext0", "Saving replaces every header: type each one's value again." }
                    }
                },
                Headers::Clear => rsx! {
                    span { class: "text-sm text-ctp-subtext0", "No headers are sent once saved." }
                    button {
                        class: "btn h-[30px] self-start",
                        r#type: "button",
                        onclick: move |_| {
                            headers.set(Headers::Keep);
                            commit();
                        },
                        "Keep the headers as they are"
                    }
                },
                Headers::Keep => rsx! {
                    span { class: "text-sm text-ctp-subtext0",
                        if set { "Set. Values are kept like passwords and never shown again." } else { "None." }
                    }
                    if !locked {
                        div { class: "flex flex-wrap gap-2",
                            button {
                                class: "btn h-[30px]",
                                r#type: "button",
                                onclick: move |_| {
                                    headers.set(Headers::Replace(vec![HeaderRow::default()]));
                                    commit();
                                },
                                icons::Plus { size: 15 }
                                if set { "Replace the headers" } else { "Add a header" }
                            }
                            if set {
                                button {
                                    class: "btn h-[30px]",
                                    r#type: "button",
                                    onclick: move |_| {
                                        headers.set(Headers::Clear);
                                        commit();
                                    },
                                    "Remove them"
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

/// `plex.token`: signing in with Plex, which keeps the token at once, or
/// a token pasted, saved with the page.
#[component]
fn PlexTokenField(field: SchemaField) -> Element {
    let key = crate::settings_layout::key(&field.key);
    let pasted = SchemaField {
        widget: FieldWidget::Secret,
        label: "Plex token".to_owned(),
        custom: None,
        ..field
    };
    let state = use_context::<AreaState>();
    let account = use_signal(|| super::wizard::Account::Idle);
    let confirming = use_signal(|| None::<String>);
    let set = state.entry(key).is_some_and(|e| e.set);
    let reload = state.reload;
    let sign_in = move |_| {
        let mut reload = reload;
        super::wizard::sign_in_with_plex(account, true, Some(confirming), move || reload += 1);
    };
    if state.locked(key) {
        return rsx! {
            span { class: "text-sm text-ctp-subtext0", "The token is read from the file the server's config names." }
        };
    }
    rsx! {
        div { class: "setting-control flex flex-col gap-3",
            super::wizard::AccountPanel {
                account: account(),
                token_set: set,
                on_sign_in: sign_in,
                on_cancel: move |_| {
                    let mut account = account;
                    account.set(super::wizard::Account::Idle);
                },
            }
            span { class: "text-xs text-ctp-subtext0", "Or paste a token, saved with the page:" }
            super::settings_area::TextBox { field: pasted }
        }
        super::profile::ConfirmItsYou {
            asking: confirming,
            on_confirmed: move |_| {
                let mut reload = reload;
                super::wizard::follow_plex_pin(account, true, Some(confirming), move || reload += 1);
            },
        }
    }
}

/// `admin.metrics_from`: who may scrape `/metrics`. Loopback stays, so a
/// scraper on this machine works; an address refused lately is offered.
#[component]
fn MetricsFromField(setting: &'static str) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let start = state
        .shown(key)
        .map(|v| crate::settings_layout::items(Some(&v)));
    let mut from = use_signal(|| allowed_from(start.as_deref()));
    let mut typed = use_signal(String::new);
    let mut refusal = use_signal(|| None::<String>);
    let status = use_resource(|| async {
        let client = rpc::client().await.ok()?;
        client.telemetry_status().await.ok()
    });
    let locked = state.locked(key);
    let commit = move || {
        let list = metrics_from_value(&from.peek());
        state.put(key, Ok(list.map(|l| json!(l))));
    };
    let refused: Vec<_> = status
        .read()
        .clone()
        .flatten()
        .map(|t| {
            refused_to_offer(&t.metrics.refused, &from())
                .into_iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    rsx! {
        div { class: "setting-control flex flex-col gap-2",
            for entry in from() {
                div { key: "{entry}", class: "flex items-center gap-2",
                    span { class: "mono-value flex-grow text-sm", "{entry}" }
                    if is_loopback(&entry) {
                        span { class: "text-xs text-ctp-subtext0", "Always" }
                    } else {
                        button {
                            class: "icon-button",
                            r#type: "button",
                            aria_label: "Remove {entry}",
                            disabled: locked,
                            onclick: {
                                let entry = entry.clone();
                                move |_| {
                                    let left = remove_address(&from.peek(), &entry);
                                    from.set(left);
                                    commit();
                                }
                            },
                            icons::Trash {}
                        }
                    }
                }
            }
            if !locked {
                form {
                    class: "flex items-center gap-2",
                    onsubmit: move |e: FormEvent| {
                        e.prevent_default();
                        let added = add_address(&from.peek(), &typed.peek());
                        match added {
                            Ok(list) => {
                                from.set(list);
                                typed.set(String::new());
                                refusal.set(None);
                                commit();
                            }
                            Err(why) => refusal.set(Some(why)),
                        }
                    },
                    input {
                        class: "setting-input",
                        aria_label: "Add an address or range",
                        placeholder: "10.10.0.0/16",
                        value: "{typed}",
                        oninput: move |e| typed.set(e.value()),
                    }
                    button {
                        class: "btn-dialog btn-dialog-grey",
                        r#type: "submit",
                        disabled: typed().trim().is_empty(),
                        icons::Plus {}
                        "Add"
                    }
                }
                if let Some(why) = refusal() {
                    span { class: "text-xs text-ctp-red", "{why}" }
                }
            }
            for r in refused {
                div { key: "{r.address}", class: "flex flex-wrap items-center gap-2",
                    span { class: "state state-warn inline-flex items-center gap-1.5 text-[13px]",
                        icons::Warning {}
                        "{refused_line(&r)}"
                    }
                    if !locked {
                        button {
                            class: "btn h-[30px]",
                            r#type: "button",
                            onclick: {
                                let address = r.address.clone();
                                move |_| {
                                    let added = add_address(&from.peek(), &address);
                                    if let Ok(list) = added {
                                        from.set(list);
                                        commit();
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
