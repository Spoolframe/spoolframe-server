//! Settings › Remote access (P6-02), as the WebRemoteAccess boards draw
//! it: what the server advertises now and how each address was found,
//! then `network.wan`'s form; and the setup wizard's "From outside your
//! home?", in the Access step's apps card (WebWizardRemoteChoices). The form's logic is
//! `crate::remote`, tested on the host.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, MappingStatus, ServerStatus, Settings};
use wasm_bindgen::JsValue;

use super::admin::sleep;
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::wizard::{Note, entry, load_settings, loading, locked, save};
use super::{Alert, Field, icons, rpc};
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

/// "Most homes don't need this": relays are the other way in.
#[component]
fn RelaysNote() -> Element {
    rsx! {
        div { class: "alert alert-info", role: "note",
            icons::Info {}
            span {
                b { "Most homes don’t need this" }
                "Few home connections have a public IP you can forward to. Relays, coming in a later version, let people connect from anywhere without any of this. Set a public address only when your connection has one: it gives a direct, faster path."
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

/// Settings › Remote access.
#[component]
pub fn RemoteAccessForm(on_done: EventHandler<()>) -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let mut status = use_signal(|| None::<ServerStatus>);
    use_future(move || async move {
        loop {
            if let Ok(now) = load_status().await {
                status.set(Some(now));
            }
            sleep(POLL_MS).await;
        }
    });
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    let saved = WanForm::from_setting(entry(&loaded, "network.wan").and_then(|e| e.value.as_ref()));
    rsx! {
        div { class: "flex flex-col gap-1.5",
            h2 { class: "m-0 text-[22px] font-semibold", "Remote access" }
            p { class: "m-0 text-sm text-ctp-subtext0",
                "Tell apps outside your home where to find this server. Signed-in devices get these addresses on their own."
            }
        }
        if let Some(now) = status().filter(|s| s.wan) {
            StatusCard { status: now, opened: saved.opened }
        }
        for g in [generation()] {
            RemoteFields {
                key: "{g}",
                settings: loaded.clone(),
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

/// A switch: its title, what it does, and the switch itself.
#[component]
fn PartHead(title: String, about: String, on: Signal<bool>, disabled: bool) -> Element {
    let mut on = on;
    rsx! {
        div { class: "wan-part-head",
            span {
                b { class: "text-[15px]", "{title}" }
                span { "{about}" }
            }
            button {
                class: "switch",
                role: "switch",
                aria_checked: "{on()}",
                aria_label: "{title}",
                disabled,
                onclick: move |_| on.toggle(),
            }
        }
    }
}

#[component]
fn RemoteFields(
    settings: Settings,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let saved =
        WanForm::from_setting(entry(&settings, "network.wan").and_then(|e| e.value.as_ref()));
    let fixed = locked(&settings, "network.wan");
    let on = use_signal(|| saved.on);
    let mapped = use_signal(|| saved.mapped);
    let port = use_signal(|| saved.port.clone());
    let mut opened = use_signal(|| saved.opened);
    let mut ip = use_signal(|| saved.ip);
    let typed_ip = use_signal(|| saved.typed_ip.clone());
    let mut lookup_service = use_signal(|| saved.lookup_service);
    let mut statics = use_signal(|| saved.statics.clone());
    let new_static = use_signal(String::new);
    let host_ipv6 = use_signal(|| saved.host_ipv6);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let found_ipv6 = use_resource(|| async {
        let client = rpc::client().await?;
        client.wan_host_ipv6().await.map_err(rpc::error)
    });
    let form = move || WanForm {
        on: on(),
        mapped: mapped(),
        port: port(),
        opened: opened(),
        ip: ip(),
        typed_ip: typed_ip(),
        lookup_service: lookup_service(),
        statics: statics(),
        host_ipv6: host_ipv6(),
    };
    let s = settings.clone();
    // `network.*` says where devices connect (finding 61): from an old
    // session, "Confirm it's you" first, then the save again.
    let confirming = use_signal(|| None::<String>);
    let on_save = use_callback(move |()| {
        let s = s.clone();
        let setting = match form().to_setting() {
            Ok(setting) => setting,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        busy.set(true);
        spawn(async move {
            match save(&s, vec![("network.wan", setting)]).await {
                Ok(()) => on_done.call(()),
                Err(e) if asks_to_confirm(&e, confirming, "changing remote access") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let adding = new_static();
    let refusal = (!adding.trim().is_empty())
        .then(|| add_static(&adding, &statics()).err())
        .flatten();
    let blocked = form().blocked();
    let server_port = super::wizard::text(&settings, "network.port");
    let port_hint = if server_port.is_empty() {
        "Default: the server’s port".to_owned()
    } else {
        format!("Default: the server’s port, {server_port}")
    };
    rsx! {
        section { class: "card tele-card",
            div { class: "wan-part",
                PartHead {
                    title: "Advertise a public address",
                    about: if on() { "On: apps outside your home are told the addresses below." } else { "Off: nothing is advertised. Clients reach the server on your home network, and through relays when they arrive." },
                    on,
                    disabled: fixed,
                }
                Note { settings: settings.clone(), setting: "network.wan", on_changed: move |_| on_reload.call(()) }
            }
            if on() {
                div { class: "wan-part",
                    PartHead {
                        title: "Mapped port",
                        about: "One public address made from a port on your router.",
                        on: mapped,
                        disabled: fixed,
                    }
                    if mapped() {
                        div { class: "wan-fields",
                            div { class: "flex flex-col gap-1.5",
                                Field { label: "Port", value: port, kind: "number", placeholder: "{server_port}", disabled: fixed || opened() == Opened::Automatic }
                                span { class: "text-xs text-ctp-subtext0",
                                    if opened() == Opened::Automatic { "Automatic maps the server’s own port." } else { "{port_hint}" }
                                }
                            }
                            div { class: "flex flex-col gap-1.5",
                                span { class: "text-xs font-semibold text-ctp-subtext0", "How the port is opened" }
                                div { class: "seg", role: "radiogroup", aria_label: "How the port is opened",
                                    for (value, label) in [(Opened::Automatic, "Automatic"), (Opened::Forwarded, "I forwarded it myself")] {
                                        button {
                                            key: "{label}",
                                            class: if opened() == value { "seg-item seg-item-on" } else { "seg-item" },
                                            role: "radio",
                                            aria_checked: "{opened() == value}",
                                            disabled: fixed,
                                            onclick: move |_| opened.set(value),
                                            "{label}"
                                        }
                                    }
                                }
                                span { class: "text-xs text-ctp-subtext0",
                                    if opened() == Opened::Automatic {
                                        "The server asks your router to open the port (UPnP, NAT-PMP or PCP) and renews it. If the router says no, nothing breaks."
                                    } else {
                                        "You forwarded the port on your router yourself. The server advertises the public IP with this port."
                                    }
                                }
                            }
                        }
                        div { class: "wan-fields",
                            span { class: "text-xs font-semibold text-ctp-subtext0 pt-2", "Public IP" }
                            div { class: "flex flex-col gap-1.5",
                                div { class: "seg", role: "radiogroup", aria_label: "Public IP",
                                    for (value, label) in [(Ip::LookUp, "Look it up"), (Ip::Typed, "This address")] {
                                        button {
                                            key: "{label}",
                                            class: if ip() == value { "seg-item seg-item-on" } else { "seg-item" },
                                            role: "radio",
                                            aria_checked: "{ip() == value}",
                                            disabled: fixed,
                                            onclick: move |_| ip.set(value),
                                            "{label}"
                                        }
                                    }
                                }
                                if ip() == Ip::Typed {
                                    Field { label: "Public IP address", value: typed_ip, placeholder: "203.0.113.24", disabled: fixed }
                                }
                                if opened() == Opened::Automatic {
                                    span { class: "text-xs text-ctp-subtext0", "Used when the router doesn’t report it (common behind carrier-grade NAT)." }
                                }
                            }
                        }
                        div { class: "wan-fields",
                            span { class: "text-xs font-semibold text-ctp-subtext0 pt-2", "Lookup service" }
                            div { class: "flex flex-col gap-1.5",
                                div { class: "seg", role: "radiogroup", aria_label: "Lookup service",
                                    for (value, label) in [(true, "wtfismyip.com"), (false, "None")] {
                                        button {
                                            key: "{label}",
                                            class: if lookup_service() == value { "seg-item seg-item-on" } else { "seg-item" },
                                            role: "radio",
                                            aria_checked: "{lookup_service() == value}",
                                            disabled: fixed,
                                            onclick: move |_| lookup_service.set(value),
                                            "{label}"
                                        }
                                    }
                                }
                                span { class: "text-xs text-ctp-subtext0", "One HTTPS request to ipv4.wtfismyip.com when the server starts. None means no outside lookups." }
                                if let Some(why) = blocked {
                                    span { class: "text-xs text-ctp-red", role: "alert", "{why}" }
                                }
                            }
                        }
                    }
                }
                div { class: "wan-part",
                    span { class: "flex flex-col gap-0.5",
                        b { class: "text-[15px]", "Static addresses" }
                        span { class: "text-[13px] text-ctp-subtext0", "Public addresses you already know. Each is advertised exactly as typed." }
                    }
                    div { class: "wan-statics",
                        for (i, address) in statics().into_iter().enumerate() {
                            div { key: "{address}", class: "wan-static",
                                span { class: "mono-value", "{address}" }
                                button {
                                    class: "icon-button",
                                    aria_label: "Remove {address}",
                                    disabled: fixed,
                                    onclick: move |_| {
                                        statics.write().remove(i);
                                    },
                                    icons::Trash {}
                                }
                            }
                        }
                        div { class: "wan-add",
                            Field { label: "Add a public address", value: new_static, placeholder: "203.0.113.7:7420", disabled: fixed, error: refusal.clone() }
                            button {
                                class: "btn-dialog btn-dialog-grey mt-[22px]",
                                disabled: fixed || adding.trim().is_empty() || refusal.is_some(),
                                onclick: move |_| {
                                    let mut new_static = new_static;
                                    if let Ok(address) = add_static(&new_static(), &statics()) {
                                        statics.write().push(address);
                                        new_static.set(String::new());
                                    }
                                },
                                icons::Plus {}
                                "Add"
                            }
                        }
                        span { class: "text-xs text-ctp-subtext0", "A public IPv4 or IPv6 with its port, such as 1.2.3.4:7420 or [2001:db8::1]:7420. Forwarding it is up to you." }
                    }
                }
                div { class: "wan-part",
                    PartHead {
                        title: "Host IPv6",
                        about: "Advertise this machine’s own stable public IPv6 addresses. No forwarding needed: only the router’s and this machine’s firewalls must let the server’s port in. Temporary privacy addresses are never used.",
                        on: host_ipv6,
                        disabled: fixed,
                    }
                    match &*found_ipv6.read() {
                        Some(Ok(found)) if !found.is_empty() => rsx! {
                            span { class: "text-xs text-ctp-subtext0", "Found: " span { class: "mono-value text-xs", {found.join(", ")} } }
                        },
                        Some(Ok(_)) => rsx! { span { class: "text-xs text-ctp-subtext0", "This machine has no stable public IPv6 address now." } },
                        _ => rsx! {},
                    }
                }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "wizard-foot",
                span { class: "wizard-foot-end",
                    button { class: "btn-dialog btn-dialog-grey", disabled: busy(), onclick: move |_| on_reload.call(()), "Discard" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy() || fixed || blocked.is_some(), onclick: move |_| on_save.call(()), "Save" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
        if on() {
            SecurityNote {}
        }
        RelaysNote {}
    }
}

/// The wizard's remote choice, as the Access step keeps it until
/// Continue saves it.
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
        let value = entry(settings, "network.wan").and_then(|e| e.value.as_ref());
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
                    "Only if your connection has a public address. Not sure? Leave Not now; relays will cover the rest in a later version. Change it any time in Settings › Remote access."
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
                        span { class: "text-xs text-ctp-subtext0", "A public IPv4 or IPv6 with its port, such as 1.2.3.4:7420 or [2001:db8::1]:7420. Forwarding it is up to you." }
                    }
                }
            }
            if choice != Choice::NotNow {
                SecurityNote {}
            }
        }
    }
}
