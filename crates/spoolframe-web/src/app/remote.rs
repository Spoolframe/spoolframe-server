//! Remote access (P6-02): what the server advertises now and how each
//! address was found, beside `network.wan`'s field; and the setup wizard's
//! "From outside your home?", in the Access step's apps card
//! (WebWizardRemoteChoices). The form's logic is `crate::remote`, tested
//! on the host.
//!
//! And how to add the server to an agent once MCP is on (PX-42), shown
//! beside the apps allowed to sign in in Settings › Sign-in & apps.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, MappingStatus, ServerStatus, Settings};
use wasm_bindgen::JsValue;

use super::admin::sleep;
use super::wizard::value;
use super::{Alert, Field, Notice, icons, plain_http_page, rpc};
use crate::logic::ApiError;
use crate::remote::{
    Choice, Ip, Line, Opened, Tone, WanForm, add_static, badge, lookup_line, mapping_line,
    service_name, source_detail, tried_line, wizard_setting,
};

/// How often the status card asks again: a mapping or a lookup may land
/// at any time.
const POLL_MS: i32 = 3_000;

const SECURITY_TITLE: &str = "The server becomes reachable from the internet";
const SECURITY_TEXT: &str = "Anyone can knock. Before signing in, strangers get tight limits: at \
                             most 64 connections, small requests, a sign-in rate limit shared by \
                             all of them, and wrong guesses counted before they are checked. Keep \
                             the server and its passwords up to date.";

/// The browser's local `HH:MM` of Unix seconds `at`.
fn local_time(at: u64) -> String {
    let date = js_sys::Date::new(&JsValue::from_f64(at as f64 * 1000.0));
    format!("{:02}:{:02}", date.get_hours(), date.get_minutes())
}

async fn load_status() -> Result<ServerStatus, ApiError> {
    let client = rpc::client().await?;
    client.server_status().await.map_err(rpc::error)
}

/// "The server becomes reachable from the internet": shown once remote
/// access is chosen.
#[component]
fn SecurityNote() -> Element {
    rsx! {
        div { class: "alert alert-warning", role: "note",
            icons::ShieldAlert {}
            span {
                b { "{SECURITY_TITLE}" }
                "{SECURITY_TEXT}"
            }
        }
    }
}

/// A status row's value, coloured by how it went.
#[component]
fn LineValue(line: Line) -> Element {
    let class = match line.tone {
        Tone::Good => "wan-good",
        Tone::Bad => "wan-bad",
        Tone::Quiet => "wan-quiet",
    };
    rsx! {
        span { class: "wan-row-value",
            span { class,
                if line.tone == Tone::Good {
                    icons::CircleCheck { size: 14 }
                    " "
                }
                "{line.text}"
            }
            if let Some(sub) = line.sub {
                small { "{sub}" }
            }
        }
    }
}

/// "Advertised now": each public address with its source's badge, then
/// the router's answer, the lookup and the reachability check.
#[component]
fn StatusCard(status: ServerStatus, opened: Opened) -> Element {
    let mapping = status.wan_mapping.clone();
    let lookup = status.wan_lookup.clone();
    rsx! {
        section { class: "card wan-card", aria_live: "polite",
            div { class: "wan-card-head",
                h2 { "Advertised now" }
                span { class: "live", span { class: "live-dot" } "Live" }
            }
            if status.public_addresses.is_empty() {
                div { class: "wan-addr",
                    span { class: "wan-quiet text-[13px]", "No public address found. Clients reach this server on the home network." }
                }
            }
            for found in status.public_addresses.clone() {
                div { key: "{found.address}", class: "wan-addr",
                    span { class: "mono-value", "{found.address}" }
                    {
                        let (label, class) = badge(found.source);
                        rsx! { span { class, "{label}" } }
                    }
                    span { class: "wan-addr-why",
                        {source_detail(found.source, &found.address, &mapping, &lookup, local_time)}
                    }
                }
            }
            div { class: "wan-row",
                span { class: "wan-row-label", "Router mapping" }
                LineValue { line: mapping_line(&mapping, opened) }
            }
            div { class: "wan-row",
                span { class: "wan-row-label", "Public IP lookup" }
                LineValue { line: lookup_line(&lookup, local_time) }
            }
            div { class: "wan-row",
                span { class: "wan-row-label", "Reachability check" }
                span { class: "wan-row-value",
                    span { class: "wan-quiet", "Not checked from here" }
                    small {
                        "Run "
                        code { class: "mono-value text-xs", "spoolframe-probe --check-wan" }
                        " from outside your home; the server cannot test its own forward."
                    }
                }
            }
        }
    }
}

/// "Advertised now", kept fresh while the page is open: only while the
/// server advertises a public address (`network.wan` saved).
#[component]
pub(super) fn RemoteStatus(opened: Opened) -> Element {
    let mut status = use_signal(|| None::<ServerStatus>);
    use_future(move || async move {
        loop {
            if let Ok(now) = load_status().await {
                status.set(Some(now));
            }
            sleep(POLL_MS).await;
        }
    });
    rsx! {
        if let Some(now) = status().filter(|s| s.wan) {
            StatusCard { status: now, opened }
        }
    }
}

/// One command to copy, with its Copy button.
#[component]
fn Command(text: String) -> Element {
    rsx! {
        div { class: "flex items-start gap-2.5",
            code { class: "mono-value cat-break min-w-0 flex-grow text-xs", "{text}" }
            super::admin::CopyButton { text: text.clone() }
        }
    }
}

/// `mcp.enabled` (PX-42): an MCP server at `/mcp`, for AI agents such
/// as Claude, which sign users in through OAuth. Off by default. While
/// it is on (as saved), how to add it to Claude Code and Codex.
/// How to add the server to Claude Code and Codex (PX-42), once MCP is
/// on as saved: an agent added before would find no server.
#[component]
pub(super) fn McpHowTo() -> Element {
    let site = use_resource(super::site);
    let public_url = site.read().as_ref().and_then(|s| s.public_url.clone());
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    let address = crate::connect::mcp_url(public_url.as_deref(), &origin);
    let claude = crate::connect::claude_code_add(&address);
    let [codex, codex_login] = crate::connect::codex_add(&address);
    rsx! {
        div { class: "flex flex-col gap-2",
            span { class: "text-[13px] text-ctp-subtext0", "Connect an agent by this address:" }
            Command { text: address.clone() }
            if public_url.is_none() {
                Notice {
                    "Over the Internet an agent needs the web UI's public address, with HTTPS (admin.public_url). Without one, only an agent that reaches this server at its own address can connect."
                }
            } else if plain_http_page() {
                Notice {
                    "This page is plain HTTP across the network: an agent's sign-in here crosses it in clear."
                }
            }
            span { class: "text-sm font-semibold", "Claude Code" }
            Command { text: claude }
            span { class: "text-[13px] text-ctp-subtext0",
                "Then run /mcp in Claude Code and choose spoolframe: it opens this server's sign-in in your browser."
            }
            span { class: "text-sm font-semibold", "Codex" }
            Command { text: codex }
            Command { text: codex_login }
            span { class: "text-[13px] text-ctp-subtext0",
                "The agent asks for library:read, playlists:read, playlists:write and, for an admin, admin. You choose which it gets on the consent page, and disconnect it in My profile."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WizardRemote {
    pub choice: Choice,
    pub port: String,
    pub ip: Ip,
    pub typed_ip: String,
    pub statics: Vec<String>,
}

impl WizardRemote {
    /// What `network.wan` says now, for a step opened again.
    pub fn from_settings(settings: &Settings) -> Self {
        let value = value(settings, "network.wan");
        let form = WanForm::from_setting(value);
        Self {
            choice: crate::remote::choice_of(value),
            port: form.port,
            ip: form.ip,
            typed_ip: form.typed_ip,
            statics: form.statics,
        }
    }

    /// The setting Continue saves; `Err` holds Continue back.
    pub fn setting(&self) -> Result<Option<serde_json::Value>, String> {
        wizard_setting(
            self.choice,
            &self.port,
            self.ip,
            &self.typed_ip,
            &self.statics,
        )
    }
}

/// "From outside your home?", in the Access step's apps card: Not now
/// writes no `network.wan`; the others open their fields and the
/// security note. Automatic asks the router at once, so the
/// answer is known before Continue.
#[component]
pub fn RemoteChoice(state: Signal<WizardRemote>, server_port: String) -> Element {
    let mut state = state;
    let mut tried = use_signal(|| None::<Result<MappingStatus, String>>);
    let mut trying = use_signal(|| false);
    let mut looked_up = use_signal(|| None::<Result<String, String>>);
    let try_router = use_callback(move |()| {
        if trying() {
            return;
        }
        trying.set(true);
        tried.set(None);
        spawn(async move {
            let asked = async {
                let client = rpc::client().await?;
                client.wan_try_mapping().await.map_err(rpc::error)
            };
            tried.set(Some(asked.await.map_err(|e| e.explain())));
            trying.set(false);
        });
    });
    let look_up = use_callback(move |()| {
        if looked_up().is_some() {
            return;
        }
        spawn(async move {
            let asked = async {
                let client = rpc::client().await?;
                client.wan_lookup().await.map_err(rpc::error)
            };
            looked_up.set(Some(asked.await.map_err(|e| e.explain())));
        });
    });
    // A step opened again with a choice made asks again at once, as a
    // click on it would.
    use_hook(move || match state.peek().choice {
        Choice::Automatic => try_router(()),
        Choice::Forwarded => look_up(()),
        Choice::NotNow | Choice::Static => {}
    });
    let choice = state.read().choice;
    let pick = use_callback(move |next: Choice| {
        state.write().choice = next;
        match next {
            Choice::Automatic => try_router(()),
            Choice::Forwarded => look_up(()),
            _ => {}
        }
    });
    let class = |here: bool| if here { "choice choice-here" } else { "choice" };
    let port_field = use_signal(|| state.read().port.clone());
    let typed_field = use_signal(|| state.read().typed_ip.clone());
    let new_static = use_signal(String::new);
    // The fields write through to the state Continue saves.
    use_effect(move || {
        let port = port_field();
        let typed = typed_field();
        let mut now = state.write();
        now.port = port;
        now.typed_ip = typed;
    });
    let adding = new_static();
    let statics = state.read().statics.clone();
    let refusal = (!adding.trim().is_empty())
        .then(|| add_static(&adding, &statics).err())
        .flatten();
    let port_shown = if server_port.is_empty() {
        "the server’s port".to_owned()
    } else {
        server_port.clone()
    };
    rsx! {
        div { class: "flex flex-col gap-3.5",
            div { class: "flex flex-col gap-1",
                div { class: "flex flex-wrap items-center gap-2.5",
                    h2 { "From outside your home?" }
                    span { class: "pill", "Optional" }
                }
                p { class: "m-0 text-[13px] leading-normal text-ctp-subtext0",
                    "Only if your connection has a public address. Not sure? Leave Not now; relays will cover the rest in a later version. Change it any time in Settings › Network."
                }
            }
            div { class: "flex flex-col gap-2", role: "radiogroup", aria_label: "Remote access",
                button { class: class(choice == Choice::NotNow), r#type: "button", role: "radio", aria_checked: "{choice == Choice::NotNow}",
                    onclick: move |_| pick(Choice::NotNow),
                    span { class: "radio" }
                    span { class: "choice-what",
                        b { "Not now" }
                        span { "Home network only, for now. Nothing is opened or looked up." }
                    }
                }
                button { class: class(choice == Choice::Automatic), r#type: "button", role: "radio", aria_checked: "{choice == Choice::Automatic}",
                    onclick: move |_| pick(Choice::Automatic),
                    span { class: "radio" }
                    span { class: "choice-what",
                        b { "My router can map it automatically" }
                        span { "The server asks the router to open a port and finds the public address." }
                    }
                }
                if choice == Choice::Automatic {
                    div { class: "flex flex-col gap-2 pl-4",
                        span { class: "text-xs text-ctp-subtext0",
                            "Port " span { class: "mono-value text-xs", "{port_shown}" } ": the server’s port. The router maps the same number outside."
                        }
                        match tried() {
                            None => rsx! { span { class: "wan-quiet text-[13px]", "Asking your router…" } },
                            Some(Err(why)) => rsx! { Alert { text: why } },
                            Some(Ok(mapping)) => {
                                let line = tried_line(&mapping);
                                rsx! {
                                    div { class: "flex flex-col gap-1",
                                        span { class: if line.tone == Tone::Good { "wan-good text-[13px]" } else if line.tone == Tone::Bad { "wan-bad text-[13px]" } else { "wan-quiet text-[13px]" }, "{line.text}" }
                                        if let Some(sub) = line.sub {
                                            span { class: "text-xs text-ctp-subtext0", "{sub}" }
                                        }
                                        span { class: "text-xs text-ctp-subtext0", "Tried just now, so you know before you continue." }
                                        button { class: "link-button self-start text-xs", disabled: trying(), onclick: move |_| try_router(()), "Try again" }
                                    }
                                }
                            }
                        }
                    }
                }
                button { class: class(choice == Choice::Forwarded), r#type: "button", role: "radio", aria_checked: "{choice == Choice::Forwarded}",
                    onclick: move |_| pick(Choice::Forwarded),
                    span { class: "radio" }
                    span { class: "choice-what",
                        b { "I forwarded a port" }
                        span { "You opened a port on your router yourself." }
                    }
                }
                if choice == Choice::Forwarded {
                    div { class: "flex flex-col gap-2.5 pl-4",
                        Field { label: "Port you forwarded", value: port_field, kind: "number", placeholder: "{server_port}" }
                        span { class: "text-xs text-ctp-subtext0 -mt-1.5", "UDP, to this machine. Empty is the server’s port." }
                        span { class: "text-xs font-semibold text-ctp-subtext0", "Public IP" }
                        div { class: "seg", role: "radiogroup", aria_label: "Public IP",
                            for (value, label) in [(Ip::LookUp, "Look it up"), (Ip::Typed, "Type it")] {
                                button {
                                    key: "{label}",
                                    class: if state.read().ip == value { "seg-item seg-item-on" } else { "seg-item" },
                                    role: "radio",
                                    aria_checked: "{state.read().ip == value}",
                                    onclick: move |_| state.write().ip = value,
                                    "{label}"
                                }
                            }
                        }
                        if state.read().ip == Ip::Typed {
                            Field { label: "Public IP address", value: typed_field, placeholder: "203.0.113.24" }
                        } else {
                            match looked_up() {
                                None => rsx! { span { class: "wan-quiet text-xs", "Asking wtfismyip.com…" } },
                                Some(Ok(ip)) => rsx! { span { class: "text-xs text-ctp-subtext0", {format!("{} says ", service_name("wtfismyip"))} span { class: "mono-value text-xs", "{ip}" } } },
                                Some(Err(why)) => rsx! { span { class: "wan-bad text-xs", "{why}" } },
                            }
                        }
                    }
                }
                button { class: class(choice == Choice::Static), r#type: "button", role: "radio", aria_checked: "{choice == Choice::Static}",
                    onclick: move |_| pick(Choice::Static),
                    span { class: "radio" }
                    span { class: "choice-what",
                        b { "I have a static IP" }
                        span { "Your provider gave you a fixed public address." }
                    }
                }
                if choice == Choice::Static {
                    div { class: "wan-statics pl-4",
                        for (i, address) in statics.clone().into_iter().enumerate() {
                            div { key: "{address}", class: "wan-static",
                                span { class: "mono-value", "{address}" }
                                button {
                                    class: "icon-button",
                                    aria_label: "Remove {address}",
                                    onclick: move |_| {
                                        state.write().statics.remove(i);
                                    },
                                    icons::Trash {}
                                }
                            }
                        }
                        div { class: "wan-add",
                            Field { label: "Public address and port", value: new_static, placeholder: "198.51.100.7:7420", error: refusal.clone() }
                            button {
                                class: "btn-dialog btn-dialog-grey mt-[22px]",
                                disabled: adding.trim().is_empty() || refusal.is_some(),
                                onclick: move |_| {
                                    let mut new_static = new_static;
                                    let known = state.read().statics.clone();
                                    if let Ok(address) = add_static(&new_static(), &known) {
                                        state.write().statics.push(address);
                                        new_static.set(String::new());
                                    }
                                },
                                icons::Plus {}
                                "Add"
                            }
                        }
                        span { class: "text-xs text-ctp-subtext0", "A public IPv4 or IPv6 with its port, such as 1.2.3.4:7420 or [2a01:4f8::1]:7420. Forwarding it is up to you." }
                    }
                }
            }
            if choice != Choice::NotNow {
                SecurityNote {}
            }
        }
    }
}
