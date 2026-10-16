//! Settings › Remote access (P6-02), as the WebRemoteAccess boards draw
//! it: what the server advertises now and how each address was found,
//! then `network.wan`'s form, and with advanced settings shown, the relays
//! and the addresses devices are told (PX-22); and the setup wizard's
//! "From outside your home?", in the Access step's apps card
//! (WebWizardRemoteChoices). The form's logic is `crate::remote`, tested
//! on the host.
//!
//! And Settings › MCP (PX-42), its own tab: the `mcp.enabled` switch and
//! how to add the server to an agent. It was a card of Remote access; it
//! stays in this file for the switch, notice and save parts it shares.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, MappingStatus, ServerStatus, Settings};
use wasm_bindgen::JsValue;

use super::admin::sleep;
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::wizard::{
    Limit, ListField, Note, entry, flag, load_settings, loading, locked, number, save, text, value,
};
use super::{Alert, Field, Notice, icons, plain_http_page, rpc};
use crate::logic::{ApiError, list_setting, string_values};
use crate::network::{Pkarr, pkarr_form, pkarr_setting, relays_form, relays_list};
use crate::remote::{
    Choice, Ip, Line, Opened, Tone, WanForm, add_static, badge, listens_on_ipv6, lookup_line,
    mapping_line, service_name, source_detail, tried_line, wizard_setting,
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
                "Few home connections have a public IP you can forward to. Relays, below, let people connect from anywhere without any of this. Set a public address only when your connection has one: it gives a direct, faster path."
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
    let advanced = crate::advanced::shown(super::settings::show_advanced(), "network.relays");
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
    let saved = WanForm::from_setting(value(&loaded, "network.wan"));
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
        for g in [generation()] {
            UplinkFields {
                key: "{g}",
                settings: loaded.clone(),
                advanced,
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
        // The relays and the addresses told are advanced (PX-22).
        if advanced {
        for g in [generation()] {
            RelayFields {
                key: "{g}",
                settings: loaded.clone(),
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
        for g in [generation()] {
            AnnounceFields {
                key: "{g}",
                settings: loaded.clone(),
                on_done,
                on_reload: move |_| generation += 1,
            }
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
    let saved = WanForm::from_setting(value(&settings, "network.wan"));
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
    let ipv6_fixed = locked(&settings, "network.ipv6");
    let ipv6 = use_signal(|| listens_on_ipv6(value(&settings, "network.ipv6")));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let form = move || WanForm {
        on: on(),
        mapped: mapped(),
        port: port(),
        opened: opened(),
        ip: ip(),
        typed_ip: typed_ip(),
        lookup_service: lookup_service(),
        statics: statics(),
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
            let changes = vec![
                ("network.wan", setting),
                ("network.ipv6", Some(serde_json::Value::Bool(ipv6()))),
            ];
            match save(&s, changes).await {
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
            }
            div { class: "wan-part",
                PartHead {
                    title: "Listen on IPv6",
                    about: if ipv6() { "On: devices may reach the server at this machine’s IPv6 addresses, directly or by hole punching." } else { "Off: the server listens on IPv4 only, so no device reaches it over IPv6." },
                    on: ipv6,
                    disabled: ipv6_fixed,
                }
                Note { settings: settings.clone(), setting: "network.ipv6", on_changed: move |_| on_reload.call(()) }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "wizard-foot",
                span { class: "wizard-foot-end",
                    button { class: "btn-dialog btn-dialog-grey", disabled: busy(), onclick: move |_| on_reload.call(()), "Discard" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy() || (fixed && ipv6_fixed) || blocked.is_some(), onclick: move |_| on_save.call(()), "Save" }
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

/// The strings of list setting `key`.
fn listed(settings: &Settings, key: &str) -> Vec<String> {
    value(settings, key)
        .map(super::wizard::strings)
        .unwrap_or_default()
}

/// A list's value at Save, beside what `key` has now.
fn list_change(settings: &Settings, key: &str, items: &[String]) -> Option<serde_json::Value> {
    list_setting(string_values(items), entry(settings, key))
}

/// Relays (P6-03, P6-04) and the pkarr record (P6-05): the other ways in
/// besides a public address, a card of their own with its own Save.
#[component]
fn RelayFields(
    settings: Settings,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let (n0_saved, urls_saved) = relays_form(value(&settings, "network.relays"));
    let relays_fixed = locked(&settings, "network.relays");
    let n0 = use_signal(|| n0_saved);
    let urls = use_signal(|| urls_saved);
    let own = use_signal(|| listed(&settings, "network.own_relays"));
    let (pkarr_saved, pkarr_url_saved) = pkarr_form(value(&settings, "network.pkarr"));
    let pkarr_fixed = locked(&settings, "network.pkarr");
    let mut pkarr = use_signal(|| pkarr_saved);
    let pkarr_url = use_signal(|| pkarr_url_saved);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let s = settings.clone();
    let confirming = use_signal(|| None::<String>);
    let on_save = use_callback(move |()| {
        let s = s.clone();
        let pkarr_value = match pkarr_setting(pkarr(), &pkarr_url()) {
            Ok(v) => v,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        let changes = vec![
            (
                "network.relays",
                list_change(&s, "network.relays", &relays_list(n0(), &urls())),
            ),
            (
                "network.own_relays",
                list_change(&s, "network.own_relays", &own()),
            ),
            ("network.pkarr", Some(pkarr_value)),
        ];
        busy.set(true);
        spawn(async move {
            match save(&s, changes).await {
                Ok(()) => on_done.call(()),
                Err(e) if asks_to_confirm(&e, confirming, "changing the relays") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let own_fixed = locked(&settings, "network.own_relays");
    rsx! {
        section { class: "card tele-card",
            div { class: "wan-part",
                PartHead {
                    title: "n0’s public relays",
                    about: if n0() { "On: devices that cannot reach the server directly go through n0’s free public relays. They pass encrypted traffic they cannot read." } else { "Off: no public relays. Devices reach the server on your network, at a public address, or through relays of your own." },
                    on: n0,
                    disabled: relays_fixed,
                }
            }
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Relays of your own" }
                    span { class: "text-[13px] text-ctp-subtext0", "The URLs of relays you run. The server takes its home relay from these and n0’s, and invites carry the list. None: relays off." }
                }
                ListField { add: "Add a relay URL", items: urls, placeholder: "https://relay.example.org", disabled: relays_fixed }
                Note { settings: settings.clone(), setting: "network.relays", on_changed: move |_| on_reload.call(()) }
            }
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Relays that let your devices through" }
                    span { class: "text-[13px] text-ctp-subtext0",
                        "The keys of relays of your own (spoolframe-server relay), so they let this server’s devices through. Print a relay’s key with "
                        span { class: "mono-value text-xs", "spoolframe-server relay id" }
                        "."
                    }
                }
                ListField { add: "Add a relay key", items: own, disabled: own_fixed }
                Note { settings: settings.clone(), setting: "network.own_relays", on_changed: move |_| on_reload.call(()) }
            }
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Find the server by its key" }
                    span { class: "text-[13px] text-ctp-subtext0",
                        "Only for a device that knows this server but no longer an address that reaches it, as when your public IP changed while it was away. The server publishes where it is, signed with its key. Not needed on your network, with a public address that does not change, or with relays on. The record is public: relays and public addresses only, never a home one."
                    }
                }
                div { class: "seg", role: "radiogroup", aria_label: "Where the record goes",
                    for (choice, label) in [(Pkarr::Off, "Off"), (Pkarr::N0, "n0 (dns.iroh.link)"), (Pkarr::Own, "My own pkarr relay")] {
                        button {
                            key: "{label}",
                            class: if pkarr() == choice { "seg-item seg-item-on" } else { "seg-item" },
                            role: "radio",
                            aria_checked: "{pkarr() == choice}",
                            disabled: pkarr_fixed,
                            onclick: move |_| pkarr.set(choice),
                            "{label}"
                        }
                    }
                }
                if pkarr() == Pkarr::Own {
                    Field { label: "Pkarr relay URL", value: pkarr_url, placeholder: "https://dns.example.org/pkarr", disabled: pkarr_fixed }
                }
                Note { settings: settings.clone(), setting: "network.pkarr", on_changed: move |_| on_reload.call(()) }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "wizard-foot",
                span { class: "wizard-foot-end",
                    button { class: "btn-dialog btn-dialog-grey", disabled: busy(), onclick: move |_| on_reload.call(()), "Discard" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy() || (relays_fixed && own_fixed && pkarr_fixed), onclick: move |_| on_save.call(()), "Save" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
    }
}

/// `network.uplink.*`, `network.lan_prefixes` and `network.wan_prefixes`
/// (PX-46): what the home's upload carries for playbacks outside it, and
/// how they share it. The ranges that count as home, or as outside, are
/// advanced.
#[component]
fn UplinkFields(
    settings: Settings,
    advanced: bool,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    const BUDGET: &str = "network.uplink.budget_mbps";
    const STREAMS: &str = "network.uplink.max_streams";
    const OWNER: &str = "network.uplink.owner_share_percent";
    const LAN: &str = "network.lan_prefixes";
    const WAN: &str = "network.wan_prefixes";
    const CONGESTION: &str = "network.congestion_control";
    let mut congestion = use_signal(|| {
        let saved = text(&settings, CONGESTION);
        if saved.is_empty() {
            "cubic".to_owned()
        } else {
            saved
        }
    });
    let mut budget = use_signal(|| text(&settings, BUDGET));
    let streams = use_signal(|| text(&settings, STREAMS));
    let owner = use_signal(|| text(&settings, OWNER));
    let lan = use_signal(|| listed(&settings, LAN));
    let wan = use_signal(|| listed(&settings, WAN));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut measuring = use_signal(|| false);
    let mut measured = use_signal(|| None::<spoolframe_admin_api::UplinkMeasurement>);
    let confirming = use_signal(|| None::<String>);
    let s = settings.clone();
    let on_save = use_callback(move |()| {
        let s = s.clone();
        let changes = (|| {
            Ok::<_, String>(vec![
                (BUDGET, number("The upload budget", &budget())?),
                (STREAMS, number("The most streams", &streams())?),
                (OWNER, number("The admins' share", &owner())?),
                (LAN, list_change(&s, LAN, &lan())),
                (WAN, list_change(&s, WAN, &wan())),
                (CONGESTION, Some(serde_json::Value::String(congestion()))),
            ])
        })();
        let changes = match changes {
            Ok(changes) => changes,
            Err(e) => {
                error.set(Some(e));
                return;
            }
        };
        busy.set(true);
        spawn(async move {
            match save(&s, changes).await {
                Ok(()) => on_done.call(()),
                Err(e) if asks_to_confirm(&e, confirming, "changing the upload budget") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
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
    let all_fixed = [BUDGET, STREAMS, OWNER, LAN, WAN, CONGESTION]
        .iter()
        .all(|k| locked(&settings, k));
    let meaning = crate::uplink::budget_meaning(&budget());
    rsx! {
        section { class: "card tele-card",
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Upload for playback outside your home" }
                    span { class: "text-[13px] text-ctp-subtext0",
                        "Devices on your home network are not counted: one in the server’s own network (its IPv6 /64 too) plays the original in Auto quality. Outside, playbacks share the budget below, and the admins’ together keep their share while they watch beside guests."
                    }
                }
            }
            div { class: "wan-part",
                Limit { label: "Upload budget", value: budget, unit: "Mbit/s", disabled: locked(&settings, BUDGET) }
                span { class: "text-[13px] text-ctp-subtext0", "{meaning}" }
                div { class: "flex flex-wrap items-center gap-2",
                    button {
                        class: "btn-dialog btn-dialog-grey",
                        disabled: measuring() || locked(&settings, BUDGET),
                        onclick: measure,
                        if measuring() { "Measuring… (about 30 MB to Cloudflare)" } else { "Measure the upload" }
                    }
                    if let Some(m) = measured() {
                        span { class: "text-[13px]", "{crate::uplink::measured_line(&m)}" }
                        button {
                            class: "btn-dialog btn-dialog-primary",
                            onclick: move |_| budget.set(m.suggested_budget_mbps.to_string()),
                            "{crate::uplink::use_suggested(&m)}"
                        }
                    }
                }
                Note { settings: settings.clone(), setting: BUDGET, on_changed: move |_| on_reload.call(()) }
            }
            div { class: "wan-part",
                Limit { label: "Most playbacks outside at once", value: streams, unit: "0 = no limit", disabled: locked(&settings, STREAMS) }
                Note { settings: settings.clone(), setting: STREAMS, on_changed: move |_| on_reload.call(()) }
            }
            div { class: "wan-part",
                Limit { label: "Admins’ share while guests watch", value: owner, unit: "%", disabled: locked(&settings, OWNER) }
                Note { settings: settings.clone(), setting: OWNER, on_changed: move |_| on_reload.call(()) }
            }
            if advanced {
                div { class: "wan-part",
                    span { class: "flex flex-col gap-0.5",
                        b { class: "text-[15px]", "Also home" }
                        span { class: "text-[13px] text-ctp-subtext0", "Ranges whose devices count as home besides the server’s own networks and private addresses (a WireGuard or Tailscale peer already is): another VLAN with public addresses." }
                    }
                    ListField { add: "Add a range", items: lan, placeholder: "203.0.113.0/24", disabled: locked(&settings, LAN) }
                    Note { settings: settings.clone(), setting: LAN, on_changed: move |_| on_reload.call(()) }
                }
                div { class: "wan-part",
                    span { class: "flex flex-col gap-0.5",
                        b { class: "text-[15px]", "Always outside" }
                        span { class: "text-[13px] text-ctp-subtext0", "Ranges whose devices count as outside your home even at a private address: a WireGuard peer that is always away. Where a range here and one under Also home both hold an address, the narrower one decides." }
                    }
                    ListField { add: "Add a range", items: wan, placeholder: "10.10.0.7/32", disabled: locked(&settings, WAN) }
                    Note { settings: settings.clone(), setting: WAN, on_changed: move |_| on_reload.call(()) }
                }
                div { class: "wan-part",
                    span { class: "flex flex-col gap-0.5",
                        b { class: "text-[15px]", "Congestion control" }
                        span { class: "text-[13px] text-ctp-subtext0", "How the server paces what it sends. Cubic backs off on every lost packet; BBR paces by the speed it measures, and keeps it on a lossy link such as cellular. It applies to every connection, home ones too, after a restart." }
                    }
                    select {
                        class: "pref-select",
                        disabled: locked(&settings, CONGESTION),
                        value: "{congestion}",
                        onchange: move |e| congestion.set(e.value()),
                        option { value: "cubic", "Cubic (the default)" }
                        option { value: "bbr", "BBR" }
                    }
                    Note { settings: settings.clone(), setting: CONGESTION, on_changed: move |_| on_reload.call(()) }
                }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "wizard-foot",
                span { class: "wizard-foot-end",
                    button { class: "btn-dialog btn-dialog-grey", disabled: busy(), onclick: move |_| on_reload.call(()), "Discard" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy() || all_fixed, onclick: move |_| on_save.call(()), "Save" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
    }
}

/// Settings › MCP.
#[component]
pub fn McpForm(on_done: EventHandler<()>) -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    rsx! {
        div { class: "flex flex-col gap-1.5",
            h2 { class: "m-0 text-[22px] font-semibold", "MCP" }
            p { class: "m-0 text-sm text-ctp-subtext0",
                "An MCP server for AI agents such as Claude Code and Codex, which act for the user who connects them."
            }
        }
        for g in [generation()] {
            McpFields {
                key: "{g}",
                settings: loaded.clone(),
                on_done,
                on_reload: move |_| generation += 1,
            }
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
#[component]
fn McpFields(
    settings: Settings,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    const KEY: &str = "mcp.enabled";
    let on = use_signal(|| flag(&settings, KEY));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // `mcp.*` is sensitive: from an old session, "Confirm it's you" first.
    let confirming = use_signal(|| None::<String>);
    let site = use_resource(super::site);
    let public_url = site.read().as_ref().and_then(|s| s.public_url.clone());
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    let address = crate::connect::mcp_url(public_url.as_deref(), &origin);
    let claude = crate::connect::claude_code_add(&address);
    let [codex, codex_login] = crate::connect::codex_add(&address);
    // The commands only once it is on, as saved: an agent added before
    // would find no server.
    let saved_on = flag(&settings, KEY);
    let s = settings.clone();
    let on_save = use_callback(move |()| {
        let s = s.clone();
        busy.set(true);
        spawn(async move {
            match save(&s, vec![(KEY, Some(serde_json::json!(on())))]).await {
                Ok(()) => on_done.call(()),
                Err(e) if asks_to_confirm(&e, confirming, "turning MCP on or off") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let fixed = locked(&settings, KEY);
    rsx! {
        section { class: "card tele-card",
            div { class: "wan-part",
                PartHead {
                    title: "AI agents (MCP)",
                    about: "Let an agent such as Claude act for a user: search the library, see what they haven't finished, make playlists, and for an admin look after the server. Each user signs in and chooses what it may do; My profile lists the apps connected, and disconnects them.",
                    on,
                    disabled: fixed,
                }
                Note { settings: settings.clone(), setting: KEY, on_changed: move |_| on_reload.call(()) }
            }
            if saved_on {
                div { class: "wan-part",
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
                }
                div { class: "wan-part",
                    span { class: "text-sm font-semibold", "Claude Code" }
                    Command { text: claude }
                    span { class: "text-[13px] text-ctp-subtext0",
                        "Then run /mcp in Claude Code and choose spoolframe: it opens this server's sign-in in your browser."
                    }
                }
                div { class: "wan-part",
                    span { class: "text-sm font-semibold", "Codex" }
                    Command { text: codex }
                    Command { text: codex_login }
                    span { class: "text-[13px] text-ctp-subtext0", "The second signs in, in your browser." }
                }
                div { class: "wan-part",
                    span { class: "text-[13px] text-ctp-subtext0",
                        "The agent asks for library:read, playlists:read, playlists:write and, for an admin, admin. You choose which it gets on the consent page, and disconnect it in My profile."
                    }
                }
            } else {
                div { class: "wan-part",
                    span { class: "text-[13px] text-ctp-subtext0",
                        "Turn MCP on and save to get the commands that add this server to Claude Code or Codex."
                    }
                }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "wizard-foot",
                span { class: "wizard-foot-end",
                    button { class: "btn-dialog btn-dialog-grey", disabled: busy(), onclick: move |_| on_reload.call(()), "Discard" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy() || fixed, onclick: move |_| on_save.call(()), "Save" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
    }
}

/// `network.announce.*` (PX-12): which of its addresses a signed-in
/// device is told.
#[component]
fn AnnounceFields(
    settings: Settings,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    const ADD: &str = "network.announce.add";
    const IGNORE: &str = "network.announce.ignore";
    const IGNORE_INTERFACES: &str = "network.announce.ignore_interfaces";
    const INTERFACES: &str = "network.announce.interfaces";
    let add = use_signal(|| listed(&settings, ADD));
    let ignore = use_signal(|| listed(&settings, IGNORE));
    let ignore_interfaces = use_signal(|| listed(&settings, IGNORE_INTERFACES));
    let interfaces = use_signal(|| listed(&settings, INTERFACES));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let s = settings.clone();
    let confirming = use_signal(|| None::<String>);
    let on_save = use_callback(move |()| {
        let s = s.clone();
        let changes = vec![
            (ADD, list_change(&s, ADD, &add())),
            (IGNORE, list_change(&s, IGNORE, &ignore())),
            (
                IGNORE_INTERFACES,
                list_change(&s, IGNORE_INTERFACES, &ignore_interfaces()),
            ),
            (INTERFACES, list_change(&s, INTERFACES, &interfaces())),
        ];
        busy.set(true);
        spawn(async move {
            match save(&s, changes).await {
                Ok(()) => on_done.call(()),
                Err(e)
                    if asks_to_confirm(
                        &e,
                        confirming,
                        "changing the addresses devices are told",
                    ) => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let all_fixed = [ADD, IGNORE, IGNORE_INTERFACES, INTERFACES]
        .iter()
        .all(|k| locked(&settings, k));
    rsx! {
        section { class: "card tele-card",
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Addresses devices are told" }
                    span { class: "text-[13px] text-ctp-subtext0",
                        "A signed-in device is told the server’s address on each network interface, but container and VM bridges (docker*, br-*, veth*, virbr*), which no device reaches. Narrow or add to that here. Interface names take * and ?."
                    }
                }
            }
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Only these interfaces" }
                    span { class: "text-[13px] text-ctp-subtext0", "None: every interface but the virtual ones. Naming a virtual one, such as docker0, tells it too." }
                }
                ListField { add: "Add an interface", items: interfaces, placeholder: "enp*", disabled: locked(&settings, INTERFACES) }
                Note { settings: settings.clone(), setting: INTERFACES, on_changed: move |_| on_reload.call(()) }
            }
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Never these interfaces" }
                    span { class: "text-[13px] text-ctp-subtext0", "Even when listed above. The virtual ones need not be listed." }
                }
                ListField { add: "Add an interface", items: ignore_interfaces, placeholder: "wg1", disabled: locked(&settings, IGNORE_INTERFACES) }
                Note { settings: settings.clone(), setting: IGNORE_INTERFACES, on_changed: move |_| on_reload.call(()) }
            }
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Never these addresses" }
                    span { class: "text-[13px] text-ctp-subtext0", "Addresses and ranges, such as 172.17.0.0/16 or 10.0.0.9." }
                }
                ListField { add: "Add an address or range", items: ignore, placeholder: "172.17.0.0/16", disabled: locked(&settings, IGNORE) }
                Note { settings: settings.clone(), setting: IGNORE, on_changed: move |_| on_reload.call(()) }
            }
            div { class: "wan-part",
                span { class: "flex flex-col gap-0.5",
                    b { class: "text-[15px]", "Also these addresses" }
                    span { class: "text-[13px] text-ctp-subtext0", "Ones the server does not see itself, such as a NAT’s public address that forwards the port: 203.0.113.7 (with the server’s port) or 203.0.113.7:7420. Told even when a rule above leaves them out." }
                }
                ListField { add: "Add an address", items: add, placeholder: "203.0.113.7:7420", disabled: locked(&settings, ADD) }
                Note { settings: settings.clone(), setting: ADD, on_changed: move |_| on_reload.call(()) }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "wizard-foot",
                span { class: "wizard-foot-end",
                    button { class: "btn-dialog btn-dialog-grey", disabled: busy(), onclick: move |_| on_reload.call(()), "Discard" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy() || all_fixed, onclick: move |_| on_save.call(()), "Save" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
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
