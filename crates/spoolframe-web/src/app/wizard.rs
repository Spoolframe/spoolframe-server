//! The setup wizard's steps 2 to 7 (P3-10), as the WebWizard boards draw
//! them, and the same forms as Server › Settings' tabs afterwards. Step 1
//! is the setup page (`super::Setup`). Each step saves what it changed in
//! the runtime settings when it is confirmed, and the server keeps which
//! steps are confirmed, so a wizard left half-way picks up where it
//! stopped.

use dioxus::prelude::*;
use serde_json::{Value, json};
use spoolframe_admin_api::{
    AdminApiClient, LibraryKind, LibraryPathCheck, MeApiClient, PathMappingEntry, PlexPinState,
    PlexStatus, ServerStatus, SettingApply, SettingEntry, SettingLayer, Settings, SetupState,
};

use super::admin::sleep;
use super::libraries::RemoteNote;
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{Alert, Field, Route, icons, now, rpc};
use crate::libraries;
use crate::logic::{ApiError, Landing, thousands};
use crate::setup::{self, Hardware, PathRow, STEPS, Sample, Step};

/// Where a form is: a wizard step (Back and Continue), or a Settings tab
/// (Discard and Save).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Wizard,
    Settings,
}

pub(super) async fn load_settings() -> Result<Settings, ApiError> {
    let client = rpc::client().await?;
    client.settings().await.map_err(rpc::error)
}

async fn load_status() -> Result<ServerStatus, ApiError> {
    let client = rpc::client().await?;
    client.server_status().await.map_err(rpc::error)
}

pub(super) async fn load_setup() -> Result<SetupState, ApiError> {
    let client = rpc::client().await?;
    client.setup_state().await.map_err(rpc::error)
}

pub(super) fn entry<'s>(settings: &'s Settings, key: &str) -> Option<&'s SettingEntry> {
    settings.entries.iter().find(|e| e.key == key)
}

/// A setting's value as a field shows it: a string as it is, a number as
/// its digits, nothing as nothing.
pub(super) fn text(settings: &Settings, key: &str) -> String {
    match entry(settings, key).and_then(|e| e.value.as_ref()) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

pub(super) fn flag(settings: &Settings, key: &str) -> bool {
    entry(settings, key)
        .and_then(|e| e.value.as_ref())
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn locked(settings: &Settings, key: &str) -> bool {
    entry(settings, key).is_some_and(|e| e.locked)
}

/// A typed text as the setting it is: empty takes the runtime value out.
fn typed(text: &str) -> Option<Value> {
    let text = text.trim();
    (!text.is_empty()).then(|| Value::String(text.to_owned()))
}

/// A typed whole number; `Err` says which field is wrong.
fn number(label: &str, text: &str) -> Result<Option<Value>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .map(|n| Some(json!(n)))
        .map_err(|_| format!("{label} is a whole number."))
}

/// Make the changes the server does not have yet: `Some` sets a setting,
/// `None` takes its runtime value out. What is unchanged, and what the
/// locked config file sets, is left alone, so the runtime settings hold
/// only what was changed here.
pub(super) async fn save(
    settings: &Settings,
    changes: Vec<(&str, Option<Value>)>,
) -> Result<(), ApiError> {
    let client = rpc::client().await?;
    for (key, value) in changes {
        let current = entry(settings, key);
        if current.is_some_and(|e| e.locked) {
            continue;
        }
        match value {
            Some(value) => {
                if current.and_then(|e| e.value.as_ref()) == Some(&value) {
                    continue;
                }
                client
                    .setting_set(key.to_owned(), value)
                    .await
                    .map_err(rpc::error)?;
            }
            None if current.is_some_and(|e| e.layer == Some(SettingLayer::Runtime)) => {
                client
                    .setting_unset(key.to_owned())
                    .await
                    .map_err(rpc::error)?;
            }
            None => {}
        }
    }
    Ok(())
}

/// "Step 3 of 7 · Plex", and a dot for each step: a tick for those
/// confirmed, the current one ringed with its title.
#[component]
pub fn Steps(current: String, done: Vec<String>) -> Element {
    let (n, step) = setup::step(&current).unwrap_or((1, STEPS[0]));
    let total = STEPS.len();
    rsx! {
        div { class: "wizard-width",
            span { class: "steps-label", "Step {n} of {total} · {step.title}" }
            ol { class: "steps", aria_label: "Setup steps",
                for (i, s) in STEPS.iter().enumerate() {
                    if i > 0 {
                        li { class: "step-line", aria_hidden: "true" }
                    }
                    li { title: "{s.title}",
                        if s.id == current {
                            span { class: "step-dot step-dot-here", aria_current: "step", "{i + 1}" }
                            span { class: "step-title", "{s.title}" }
                        } else if done.iter().any(|d| d == s.id) {
                            span { class: "step-dot step-dot-done", icons::Check {} }
                        } else {
                            span { class: "step-dot", "{i + 1}" }
                        }
                    }
                }
            }
        }
    }
}

/// The wizard's page: the colour blobs, the wordmark, the steps, a card.
#[component]
pub fn WizardLayout(children: Element) -> Element {
    rsx! {
        main { class: "relative min-h-screen overflow-hidden bg-ctp-base text-ctp-text",
            div { class: "blobs", aria_hidden: "true",
                div { class: "blob-teal" }
                div { class: "blob-blue" }
                div { class: "blob-mauve" }
            }
            div { class: "wizard-page",
                div { class: "flex items-center gap-2.5",
                    icons::Mark { size: 40 }
                    super::Wordmark { class: "text-[24px]" }
                }
                {children}
                span { class: "wizard-tail",
                    "Each step is saved when you continue. Close the page any time and pick up here."
                }
            }
        }
    }
}

/// The row under a form: Back and Continue in the wizard, Discard and Save
/// in Settings.
#[component]
pub(super) fn Footer(
    mode: Mode,
    step: &'static str,
    busy: bool,
    on_save: EventHandler<()>,
    on_discard: EventHandler<()>,
    #[props(default)] blocked: Option<String>,
) -> Element {
    match mode {
        Mode::Wizard => rsx! {
            div { class: "wizard-foot",
                match setup::previous(step).filter(|s| s.id != "admin") {
                    Some(back) => rsx! {
                        Link { class: "btn-plain inline-flex items-center", to: Route::Wizard { step: back.id.to_owned() }, "Back" }
                    },
                    None => rsx! { span {} },
                }
                span { class: "wizard-foot-end",
                    if let Some(why) = &blocked {
                        span { "{why}" }
                    }
                    button {
                        class: "btn-dialog btn-dialog-primary h-10",
                        disabled: busy || blocked.is_some(),
                        onclick: move |_| on_save.call(()),
                        icons::ArrowRight {}
                        "Continue"
                    }
                }
            }
        },
        Mode::Settings => rsx! {
            div { class: "wizard-foot",
                span { class: "wizard-foot-end",
                    button { class: "btn-dialog btn-dialog-grey", disabled: busy, onclick: move |_| on_discard.call(()), "Discard" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy, onclick: move |_| on_save.call(()), "Save" }
                }
            }
        },
    }
}

/// Under a field: that the locked config file sets it, that it overrides
/// the file's value (with a way back to it), or that it applies after a
/// restart, and once changed, a way to restart where the server can.
#[component]
pub(super) fn Note(
    settings: Settings,
    setting: &'static str,
    on_changed: EventHandler<()>,
) -> Element {
    let key = setting;
    let mut restarting = use_signal(|| None::<String>);
    // A sensitive setting (`admin.*`, `auth.*`, `media.*`, `telemetry.*`,
    // `plex.token`, `plex.token_file`, `plex.url`, `plex.servers`), from
    // an old session: "Confirm it's you" first, then the same change
    // again.
    let confirming = use_signal(|| None::<String>);
    let use_file = use_callback(move |()| {
        spawn(async move {
            let unset = async {
                let client = rpc::client().await?;
                client
                    .setting_unset(key.to_owned())
                    .await
                    .map_err(rpc::error)
            };
            match unset.await {
                Ok(_) => on_changed.call(()),
                Err(e) => {
                    asks_to_confirm(&e, confirming, "changing this setting");
                }
            }
        });
    });
    let Some(e) = entry(&settings, key).cloned() else {
        return rsx! {};
    };
    let file = settings.config_file.clone().unwrap_or_default();
    let restart = move |_| {
        restarting.set(Some("Restarting…".into()));
        spawn(async move {
            let asked = async {
                let client = rpc::client().await?;
                client.restart().await.map_err(rpc::error)
            };
            if let Err(e) = asked.await {
                restarting.set(Some(e.explain()));
                return;
            }
            // Back when the listener answers again; then the page is read
            // anew.
            sleep(1500).await;
            for _ in 0..60 {
                if super::http::site().await.is_ok() {
                    rpc::forget();
                    if let Some(window) = web_sys::window() {
                        let _ = window.location().reload();
                    }
                    return;
                }
                sleep(1000).await;
            }
            restarting.set(Some("The server has not come back. Check its log.".into()));
        });
    };
    rsx! {
        if e.locked {
            span { class: "note",
                icons::Lock {}
                b { "Set by the server's config file" }
                span { class: "mono-value text-xs", "{file}" }
            }
        } else if let Some(overridden) = &e.overrides {
            span { class: "note",
                "Overrides {shown(overridden)} from "
                span { class: "mono-value text-xs", "{file}" }
                " · "
                button { onclick: move |_| use_file.call(()), "Use the file's value" }
            }
        }
        if e.apply == SettingApply::Restart && !e.locked {
            span { class: "note note-restart", icons::RotateCcw { size: 13 } "Applies after a restart" }
        }
        if e.restart_pending {
            div { class: "restart-box",
                span {
                    match restarting() {
                        Some(said) => rsx! { "{said}" },
                        None => rsx! { "Saved. The server still runs with the value it started with until it restarts." },
                    }
                }
                if settings.can_restart && restarting().is_none() {
                    button { class: "btn btn-teal", onclick: restart, icons::RotateCcw { size: 13 } "Restart now" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| use_file.call(()) }
    }
}

/// A value as a note quotes it.
fn shown(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A form's loaded settings, or why they did not load.
pub(super) fn loading(state: &Option<Result<Settings, ApiError>>) -> Option<Element> {
    match state {
        None => Some(rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } }),
        Some(Err(e)) => Some(rsx! { Alert { text: e.explain() } }),
        Some(Ok(_)) => None,
    }
}

/// Step 2: the server's name, and its fingerprint to note down.
#[component]
pub fn ServerForm(mode: Mode, on_done: EventHandler<()>) -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let status = use_resource(load_status);
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    rsx! {
        for g in [generation()] {
            ServerFields {
                key: "{g}",
                settings: loaded.clone(),
                status: status.read().clone().and_then(Result::ok),
                mode,
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

#[component]
fn ServerFields(
    settings: Settings,
    status: Option<ServerStatus>,
    mode: Mode,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let name = use_signal(|| text(&settings, "name"));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let s = settings.clone();
    let on_save = move |_| {
        let s = s.clone();
        busy.set(true);
        spawn(async move {
            match save(&s, vec![("name", typed(&name()))]).await {
                Ok(()) => on_done.call(()),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let fingerprint = status.as_ref().and_then(|s| s.fingerprint.clone());
    let key = status.as_ref().and_then(|s| s.endpoint_id.clone());
    rsx! {
        if mode == Mode::Wizard {
            div { class: "flex flex-col gap-1.5",
                h1 { "Name this server" }
                p { class: "lede", "People see the name when they pick a server in the apps." }
            }
        }
        div { class: "flex flex-col gap-1.5",
            Field { label: "Server name", value: name, tall: true, disabled: locked(&settings, "name") }
            Note { settings: settings.clone(), setting: "name", on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-2.5",
            div { class: "flex items-baseline justify-between gap-3",
                h2 { "Fingerprint" }
                if let Some(fp) = fingerprint.clone() {
                    super::admin::CopyButton { text: fp }
                }
            }
            div { class: "fingerprint-box",
                match fingerprint {
                    Some(fp) => rsx! { div { class: "fingerprint-big", "{fp}" } },
                    None => rsx! { span { "Starting…" } },
                }
                span { "Note this down. An app shows it the first time it connects, so you can check it's talking to this server." }
                if let Some(key) = key {
                    details { class: "self-stretch text-xs",
                        summary { class: "cursor-pointer", "Full key" }
                        div { class: "mt-2 mono-value text-xs", "{key}" }
                    }
                }
            }
        }
        if let Some(text) = error() {
            Alert { text }
        }
        Footer { mode, step: "server", busy: busy(), on_save, on_discard: move |_| on_reload.call(()) }
    }
}

/// How the Plex account step is going.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Account {
    /// Nothing done here yet; `set` says whether the server has a token.
    Idle,
    /// Plex's sign-in is open, plex.tv not answered yet. `opened`: whether
    /// its window opened. The PIN is a strong one, with no code to show:
    /// it is only ever typed into plex.tv by the address.
    Waiting {
        id: u64,
        auth_url: String,
        expires_at: Option<i64>,
        opened: bool,
    },
    /// plex.tv asked for a PIN, not answered yet.
    Asking,
    Linked(Option<String>),
    Expired,
    Failed(String),
}

/// Sign in with Plex (plex.tv's PIN flow), from a click: Plex's page
/// opens in a new window, and `account` follows plex.tv until the PIN is
/// approved, expires or fails; `on_linked` runs once it is approved. With
/// `keep_token` the server keeps the account's token as the first
/// server's (the Plex step); without, only to add servers with (P3-18).
/// Keeping it needs a recent sign-in (finding 44): from an old session,
/// `confirming` opens "Confirm it's you" and the PIN waits, still
/// approved, for [`follow_plex_pin`] to take up again.
pub(super) fn sign_in_with_plex(
    mut account: Signal<Account>,
    keep_token: bool,
    confirming: Option<Signal<Option<String>>>,
    on_linked: impl FnOnce() + 'static,
) {
    // Opened now, from the click: a tab opened after an answer from the
    // server is one a browser may block.
    let tab =
        web_sys::window().and_then(|w| w.open_with_url_and_target("", "_blank").ok().flatten());
    account.set(Account::Asking);
    spawn(async move {
        let begun = async {
            let client = rpc::client().await?;
            client.plex_pin_begin().await.map_err(rpc::error)
        };
        let pin = match begun.await {
            Ok(pin) => pin,
            Err(e) => {
                if let Some(tab) = &tab {
                    let _ = tab.close();
                }
                account.set(Account::Failed(e.explain()));
                return;
            }
        };
        let opened = tab
            .as_ref()
            .is_some_and(|t| t.location().set_href(&pin.auth_url).is_ok());
        account.set(Account::Waiting {
            id: pin.id,
            auth_url: pin.auth_url,
            expires_at: pin.expires_in.map(|s| now() + s as i64),
            opened,
        });
        follow_plex_pin(account, keep_token, confirming, on_linked);
    });
}

/// Follow the PIN `account` is waiting on, as [`sign_in_with_plex`] does,
/// until it is approved, expires or fails; nothing if it waits on none.
pub(super) fn follow_plex_pin(
    mut account: Signal<Account>,
    keep_token: bool,
    confirming: Option<Signal<Option<String>>>,
    on_linked: impl FnOnce() + 'static,
) {
    let Account::Waiting { id, .. } = *account.peek() else {
        return;
    };
    spawn(async move {
        loop {
            sleep(2000).await;
            if !matches!(&*account.peek(), Account::Waiting { id: waiting, .. } if *waiting == id) {
                return;
            }
            let checked = async {
                let client = rpc::client().await?;
                client
                    .plex_pin_check(id, keep_token)
                    .await
                    .map_err(rpc::error)
            };
            match checked.await {
                Ok(PlexPinState::Waiting) => {}
                Ok(PlexPinState::Linked { account: who }) => {
                    account.set(Account::Linked(who));
                    on_linked();
                    return;
                }
                Ok(PlexPinState::Expired) => {
                    account.set(Account::Expired);
                    return;
                }
                // Still waiting, until "Confirm it's you" takes it up.
                Err(e)
                    if confirming.is_some_and(|c| {
                        asks_to_confirm(&e, c, "keeping your Plex account's token")
                    }) =>
                {
                    return;
                }
                Err(e) => {
                    account.set(Account::Failed(e.explain()));
                    return;
                }
            }
        }
    });
}

/// Step 3: which Plex Media Server, and its token, by signing in with Plex
/// or pasting one. The libraries it has are listed.
#[component]
pub fn PlexForm(mode: Mode, on_done: EventHandler<()>) -> Element {
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
        for g in [generation()] {
            PlexFields {
                key: "{g}",
                settings: loaded.clone(),
                mode,
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

/// The PMS at `url` now, with the token the server has.
async fn probe(url: String, token: Option<String>) -> Result<PlexStatus, ApiError> {
    let client = rpc::client().await?;
    client.plex_probe(url, token).await.map_err(rpc::error)
}

#[component]
fn PlexFields(
    settings: Settings,
    mode: Mode,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let saved_url = text(&settings, "plex.url");
    let url_locked = locked(&settings, "plex.url");
    let token_entry = entry(&settings, "plex.token").cloned();
    let token_locked = token_entry.as_ref().is_some_and(|e| e.locked);
    let found = use_resource(|| async {
        let client = rpc::client().await?;
        client.plex_discover().await.map_err(rpc::error)
    });
    // The servers of the Plex account signed in with (or the admin's
    // linked one), asked again after each sign-in. None until either:
    // the server says so, and nothing more is shown.
    let mut signed_in = use_signal(|| 0u32);
    let from_account = use_resource(move || async move {
        let _ = signed_in();
        let client = rpc::client().await?;
        client.plex_account_servers().await.map_err(rpc::error)
    });
    // Found and listed, each server once (P3-10).
    let choices = use_memo(move || {
        let found = match &*found.read() {
            Some(Ok(list)) => list.clone(),
            _ => Vec::new(),
        };
        let listed = match &*from_account.read() {
            Some(Ok(list)) => list.clone(),
            _ => Vec::new(),
        };
        libraries::plex_choices(&found, &listed)
    });
    // The address chosen: one found, or typed ("" until something is).
    let mut chosen = use_signal(|| saved_url.clone());
    let mut typing = use_signal(|| false);
    let mut typed_url = use_signal(|| saved_url.clone());
    let mut probed = use_signal(|| None::<Result<PlexStatus, ApiError>>);
    let mut probes = use_signal(|| 0u32);
    let mut tab = use_signal(|| 0u8);
    let mut account = use_signal(|| Account::Idle);
    let pasted = use_signal(String::new);
    let mut paste_error = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // The library sections checked (P3-11), as of the server last probed.
    let mut shared = use_signal(Vec::<String>::new);
    use_effect(move || {
        if let Some(Ok(p)) = &*probed.read()
            && p.reachable
        {
            shared.set(libraries::checked(&p.libraries));
        }
    });

    // Probe the chosen address again whenever it or the token changes.
    let mut reprobe = move || {
        let url = if typing() { typed_url() } else { chosen() };
        probes += 1;
        let n = probes();
        if url.trim().is_empty() {
            probed.set(None);
            return;
        }
        probed.set(None);
        spawn(async move {
            // A typed address is asked about once the typing stops.
            sleep(500).await;
            if probes() != n {
                return;
            }
            let result = probe(url, None).await;
            if probes() == n {
                probed.set(Some(result));
            }
        });
    };
    use_hook(reprobe);
    // Nothing chosen yet: the first server found or listed, as they
    // come; never a remote one, whose files are here only if mounted.
    use_effect(move || {
        let choices = choices();
        if chosen.peek().is_empty()
            && !*typing.peek()
            && let Some(first) = libraries::first_choice(&choices)
        {
            chosen.set(first);
            reprobe();
        }
    });
    // A saved address neither discovery nor the account lists is a typed
    // one, once both have answered.
    use_effect(move || {
        let choices = choices();
        if found.read().is_some() && from_account.read().is_some() {
            let saved = chosen.peek().clone();
            if !saved.is_empty() && !choices.iter().any(|c| c.url == saved) {
                typing.set(true);
            }
        }
    });

    // Each way to `plex.token` needs a recent sign-in (finding 44): from
    // an old session, "Confirm it's you", then the same again.
    let confirming = use_signal(|| None::<String>);
    let mut again = use_signal(|| None::<Again>);
    let on_linked = move || {
        signed_in += 1;
        reprobe();
    };
    let sign_in = move |_| {
        again.set(Some(Again::SignIn));
        sign_in_with_plex(account, true, Some(confirming), on_linked);
    };
    let check_token = use_callback(move |()| {
        let url = if typing() { typed_url() } else { chosen() };
        let token = pasted().trim().to_owned();
        if token.is_empty() {
            return;
        }
        paste_error.set(None);
        spawn(async move {
            match probe(url, Some(token.clone())).await {
                Ok(status) if status.reachable => {
                    let kept = async {
                        let client = rpc::client().await?;
                        client
                            .setting_set("plex.token".into(), json!(token))
                            .await
                            .map_err(rpc::error)
                    };
                    match kept.await {
                        Ok(_) => {
                            account.set(Account::Linked(None));
                            probed.set(Some(Ok(status)));
                        }
                        Err(e) if asks_to_confirm(&e, confirming, "keeping a Plex token") => {
                            again.set(Some(Again::Paste));
                        }
                        Err(e) => paste_error.set(Some(e.explain())),
                    }
                }
                Ok(status) => paste_error.set(status.error),
                Err(e) => paste_error.set(Some(e.explain())),
            }
        });
    });
    let s = settings.clone();
    let on_save = use_callback(move |()| {
        let s = s.clone();
        let url = if typing() { typed_url() } else { chosen() };
        // One of the account's: the token plex.tv gives the account for
        // it, which a server shared with it needs, is kept first.
        let listed = !typing()
            && choices
                .peek()
                .iter()
                .any(|c| c.url == url && c.account.is_some());
        busy.set(true);
        spawn(async move {
            let changes = vec![("plex.url", typed(&url))];
            let token = async {
                if listed {
                    let client = rpc::client().await?;
                    client
                        .plex_account_token_use(url.clone())
                        .await
                        .map_err(rpc::error)?;
                }
                Ok::<(), ApiError>(())
            };
            let saved = match token.await {
                Ok(()) => save(&s, changes).await,
                Err(e) => Err(e),
            };
            let saved = match saved {
                Ok(()) => choose_libraries(shared.peek().clone()).await,
                Err(e) => Err(e),
            };
            match saved {
                Ok(()) => on_done.call(()),
                // `plex.url` too says where the kept token goes (finding 47).
                Err(e) if asks_to_confirm(&e, confirming, "saving the Plex server") => {
                    again.set(Some(Again::Save));
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let on_confirmed = move |()| match again.take() {
        Some(Again::SignIn) => follow_plex_pin(account, true, Some(confirming), on_linked),
        Some(Again::Paste) => check_token.call(()),
        Some(Again::Save) => on_save.call(()),
        None => {}
    };

    let reachable = probed()
        .as_ref()
        .is_some_and(|p| p.as_ref().is_ok_and(|s| s.reachable));
    // Plex's own words for a token it refuses; one it needs and has not
    // got is not an error yet.
    let rejected = probed()
        .as_ref()
        .and_then(|p| p.as_ref().ok())
        .and_then(|s| s.error.clone())
        .filter(|e| e.contains("rejected the token"));
    let token_set =
        token_entry.as_ref().is_some_and(|e| e.set) || matches!(account(), Account::Linked(_));
    let blocked = (mode == Mode::Wizard && !reachable)
        .then(|| "Plex has to answer before going on".to_owned());
    let found_list = found.read().clone();
    let choice_list = choices();
    rsx! {
        if mode == Mode::Wizard {
            div { class: "flex flex-col gap-1.5",
                h1 { "Connect to Plex" }
                p { class: "lede", "Spoolframe reads your libraries from Plex and plays the files directly." }
            }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Plex Media Server" }
            if url_locked {
                div { class: "panel",
                    b { "{saved_url}" }
                    Note { settings: settings.clone(), setting: "plex.url", on_changed: move |_| on_reload.call(()) }
                }
            } else {
                match found_list {
                    None => rsx! {
                        div { class: "flex items-center gap-2 text-sm text-ctp-subtext0", role: "status",
                            span { class: "flex animate-spin", icons::Loader {} }
                            "Looking for Plex on this network…"
                        }
                    },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div { class: "flex flex-col gap-1 text-sm text-ctp-subtext0",
                            span { class: "flex items-center gap-2", icons::CircleDashed {} "No Plex servers answered on this network" }
                            span { "Discovery only sees servers on the same network segment. Type the address instead." }
                        }
                    },
                    Some(Err(e)) => rsx! { span { class: "text-sm text-ctp-subtext0", "{e.explain()}" } },
                    Some(Ok(_)) => rsx! {},
                }
                for server in choice_list {
                    button {
                        key: "{server.url}",
                        class: if !typing() && chosen() == server.url { "choice choice-here" } else { "choice" },
                        onclick: {
                            let url = server.url.clone();
                            move |_| {
                                typing.set(false);
                                chosen.set(url.clone());
                                reprobe();
                            }
                        },
                        span { class: "radio" }
                        span { class: "choice-what",
                            b { "{server.name}" }
                            {libraries::choice_line(&server)}
                            if server.remote() {
                                RemoteNote {}
                            }
                        }
                        if !typing() && chosen() == server.url {
                            ProbeState { probed: probed() }
                        }
                    }
                }
                button {
                    class: if typing() { "choice choice-here" } else { "choice" },
                    onclick: move |_| {
                        typing.set(true);
                        reprobe();
                    },
                    span { class: "radio" }
                    span { class: "choice-what",
                        b { "Type an address" }
                        "For a server discovery didn't find, or one on another network"
                        if typing() && libraries::remote(&typed_url(), None) {
                            RemoteNote {}
                        }
                    }
                }
                if typing() {
                    label { class: "field",
                        "Address"
                        input {
                            r#type: "url",
                            placeholder: "http://host:32400",
                            value: "{typed_url}",
                            oninput: move |e| {
                                typed_url.set(e.value());
                                reprobe();
                            },
                        }
                    }
                    match probed() {
                        Some(Ok(p)) if p.reachable => rsx! {
                            span { class: "state state-ok flex items-center gap-1.5 text-sm",
                                icons::CircleCheck {}
                                {reachable_line(&p)}
                            }
                        },
                        Some(Ok(p)) if !needs_token(&p) => rsx! {
                            div { class: "alert alert-error", role: "alert",
                                icons::CircleX {}
                                span {
                                    b { {format!("Plex cannot be reached: {}", p.error.clone().unwrap_or_default())} }
                                    "Check the address and that Plex is running. The port is usually 32400."
                                }
                            }
                        },
                        Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                        _ => rsx! {},
                    }
                }
                span { class: "text-xs text-ctp-subtext0", "{libraries::FILES_HERE}" }
            }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Plex account" }
            if token_locked {
                div { class: "panel",
                    span { "The token is read from the file the server's config names." }
                    Note { settings: settings.clone(), setting: "plex.token", on_changed: move |_| on_reload.call(()) }
                }
            } else {
                div { class: "tabs", role: "tablist",
                    button { class: if tab() == 0 { "tab tab-here" } else { "tab" }, role: "tab", onclick: move |_| tab.set(0), "Sign in with Plex" }
                    button { class: if tab() == 1 { "tab tab-here" } else { "tab" }, role: "tab", onclick: move |_| tab.set(1), "Paste a token" }
                }
                if let Some(why) = &rejected {
                    div { class: "alert alert-error", role: "alert",
                        icons::CircleX {}
                        span {
                            b { "Plex rejected the token" }
                            "{why}. It may have been revoked. Sign in with Plex again or paste a new one."
                        }
                    }
                }
                if tab() == 0 {
                    AccountPanel { account: account(), token_set, on_sign_in: sign_in, on_cancel: move |_| account.set(Account::Idle) }
                } else {
                    div { class: "panel",
                        // In a form of its own, as browsers expect of a
                        // password field; Enter does nothing.
                        form { class: "contents", onsubmit: move |e: FormEvent| e.prevent_default(),
                            label { class: "field",
                                "Plex token"
                                input {
                                    r#type: "password",
                                    autocomplete: "off",
                                    value: "{pasted}",
                                    oninput: {
                                        let mut pasted = pasted;
                                        move |e: FormEvent| pasted.set(e.value())
                                    },
                                }
                            }
                        }
                        span { "From Plex Web: any item › Get Info › View XML; the token is in the address." }
                        if let Some(text) = paste_error() {
                            Alert { text }
                        }
                        if matches!(account(), Account::Linked(None)) {
                            span { class: "state state-ok flex items-center gap-1.5", icons::CircleCheck {} "Token kept" }
                        }
                        button { class: "btn btn-teal h-9", disabled: pasted().trim().is_empty(), onclick: move |_| check_token.call(()), "Check token" }
                    }
                }
            }
        }
        if let Some(Ok(p)) = probed().filter(|p| p.as_ref().is_ok_and(|s| s.reachable)) {
            div { class: "flex flex-col gap-1",
                div { class: "flex items-baseline justify-between gap-3",
                    h2 { "Libraries" }
                    span { class: "text-xs text-ctp-subtext0",
                        {format!("{} of {} shared with Spoolframe", shared().len(), p.libraries.len() + p.unsupported.len())}
                    }
                }
                div { class: "lines",
                    for library in p.libraries.clone() {
                        div { key: "{library.section}", class: "library-line",
                            button {
                                class: if shared().contains(&library.section) { "box box-on" } else { "box" },
                                role: "checkbox",
                                aria_checked: "{shared().contains(&library.section)}",
                                aria_label: "Share {library.title}",
                                disabled: url_locked,
                                onclick: {
                                    let section = library.section.clone();
                                    move |_| shared.with_mut(|c| {
                                        match c.iter().position(|s| *s == section) {
                                            Some(i) => {
                                                c.remove(i);
                                            }
                                            None => c.push(section.clone()),
                                        }
                                    })
                                },
                                if shared().contains(&library.section) { icons::Check {} }
                            }
                            b { class: "flex-grow", "{library.title}" }
                            span { class: "quiet", {format!("{} · {}", kind_name(library.kind), thousands(library.items as u64))} }
                        }
                    }
                    for other in p.unsupported.clone() {
                        div { key: "{other.title}", class: "library-line off",
                            button { class: "box", role: "checkbox", aria_checked: "false", disabled: true }
                            b { class: "flex-grow", "{other.title}" }
                            span { class: "quiet", "Not served by Spoolframe" }
                        }
                    }
                }
            }
        }
        div { class: "flex flex-wrap items-center gap-3",
            Link {
                class: "btn",
                to: Route::AddLibrary { back: if mode == Mode::Wizard { "wizard".into() } else { "settings".into() } },
                icons::Plus { size: 14 }
                "Add another Plex server"
            }
            span { class: "text-xs text-ctp-subtext0", "Libraries from several Plex servers show up as one set in the apps." }
        }
        if let Some(text) = error() {
            Alert { text }
        }
        Footer { mode, step: "plex", busy: busy(), on_save, on_discard: move |_| on_reload.call(()), blocked }
        ConfirmItsYou { asking: confirming, on_confirmed }
    }
}

/// What the Plex step makes again once "Confirm it's you" is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Again {
    /// Take up the PIN signed in with, whose token is to be kept.
    SignIn,
    /// Keep the token pasted.
    Paste,
    /// Save, keeping the chosen server's token from the account.
    Save,
}

/// Make the first Plex server's libraries the sections `chosen`: those
/// not yet libraries added, the others removed (their watch state kept).
/// Asked of the server after its address is saved, so what is a library
/// already is what it serves now.
async fn choose_libraries(chosen: Vec<String>) -> Result<(), ApiError> {
    let client = rpc::client().await?;
    let status = client.plex_status().await.map_err(rpc::error)?;
    if !status.reachable {
        return Ok(());
    }
    let change = libraries::choice(&status.libraries, &chosen);
    for section in change.add {
        client
            .library_add("plex".into(), section, None)
            .await
            .map_err(rpc::error)?;
    }
    for id in change.remove {
        client.library_remove(id).await.map_err(rpc::error)?;
    }
    Ok(())
}

/// "Reachable · Plex Media Server 1.41 · “Cabin”".
fn reachable_line(p: &PlexStatus) -> String {
    let mut line = "Reachable".to_owned();
    if let Some(v) = &p.version {
        line.push_str(&format!(
            " · Plex Media Server {}",
            crate::logic::plex_version(v)
        ));
    }
    if let Some(name) = &p.server_name {
        line.push_str(&format!(" · “{name}”"));
    }
    line
}

fn kind_name(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Movies => "Movies",
        LibraryKind::Shows => "Shows",
        LibraryKind::Music => "Music",
    }
}

/// Whether Plex answered, but wants a token, or another one.
fn needs_token(p: &PlexStatus) -> bool {
    p.error
        .as_deref()
        .is_some_and(|e| e.contains("requires a token") || e.contains("rejected the token"))
}

/// Where the chosen server's probe is.
#[component]
fn ProbeState(probed: Option<Result<PlexStatus, ApiError>>) -> Element {
    match probed {
        None => rsx! { span { class: "flex animate-spin text-ctp-blue", icons::Loader {} } },
        Some(Ok(p)) if p.reachable => rsx! {
            span { class: "state state-ok flex items-center gap-1.5 text-[13px]", icons::CircleCheck {} "Reachable" }
        },
        Some(Ok(p)) if needs_token(&p) => rsx! {
            span { class: "state state-warn flex items-center gap-1.5 text-[13px]", icons::Key { size: 14 } "Needs a token" }
        },
        Some(Ok(_)) | Some(Err(_)) => rsx! {
            span { class: "state state-bad flex items-center gap-1.5 text-[13px]", icons::CircleX {} "Not reachable" }
        },
    }
}

/// The account half of the Plex step, signing in with Plex.
#[component]
pub(super) fn AccountPanel(
    account: Account,
    token_set: bool,
    on_sign_in: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    match account {
        Account::Waiting {
            auth_url,
            expires_at,
            opened,
            ..
        } => {
            let left = expires_at.map(|at| (at - now()).max(0));
            rsx! {
                div { class: "panel items-center text-center",
                    span {
                        if opened {
                            "A Plex window opened. Sign in there and allow Spoolframe."
                        } else {
                            "Open the Plex window, sign in there and allow Spoolframe."
                        }
                    }
                    span { class: "flex items-center gap-2 text-ctp-blue", role: "status",
                        span { class: "flex animate-spin", icons::Loader { size: 14 } }
                        match left {
                            Some(s) => rsx! { {format!("Waiting for plex.tv · {}:{:02} left", s / 60, s % 60)} },
                            None => rsx! { "Waiting for plex.tv" },
                        }
                    }
                    div { class: "flex flex-wrap justify-center gap-2.5",
                        button { class: "btn", onclick: move |_| { super::plex::open(&auth_url); },
                            icons::ExternalLink { size: 15 }
                            if opened { "Open the Plex window again" } else { "Open the Plex window" }
                        }
                        button { class: "btn-plain", onclick: move |_| on_cancel.call(()), "Cancel" }
                    }
                    span { class: "text-xs text-ctp-subtext0", "If no window appeared, your browser may have blocked it." }
                }
            }
        }
        Account::Linked(who) => rsx! {
            div { class: "panel flex-row flex-wrap items-center",
                span { class: "flex text-ctp-green", icons::CircleCheck { size: 18 } }
                span { class: "flex min-w-0 flex-grow flex-col gap-0.5",
                    b {
                        match who {
                            Some(who) => rsx! { "Signed in as {who}" },
                            None => rsx! { "Token kept" },
                        }
                    }
                    "The token is kept in its own file on the server, not in the settings."
                }
                button { class: "btn", onclick: move |_| on_sign_in.call(()), "Use another account" }
            }
        },
        Account::Expired => rsx! {
            div { class: "panel",
                span { "The code expired before it was approved." }
                button { class: "btn-dialog btn-dialog-primary", onclick: move |_| on_sign_in.call(()), "Sign in with Plex" }
            }
        },
        Account::Failed(why) => rsx! {
            div { class: "panel",
                Alert { text: why }
                button { class: "btn-dialog btn-dialog-primary", onclick: move |_| on_sign_in.call(()), "Sign in with Plex" }
            }
        },
        Account::Asking => rsx! {
            div { class: "panel items-center",
                span { class: "flex items-center gap-2", role: "status",
                    span { class: "flex animate-spin", icons::Loader { size: 14 } }
                    "Asking plex.tv for a code…"
                }
            }
        },
        Account::Idle if token_set => rsx! {
            div { class: "panel flex-row flex-wrap items-center",
                span { class: "flex text-ctp-green", icons::CircleCheck { size: 18 } }
                span { class: "flex min-w-0 flex-grow flex-col gap-0.5",
                    b { "A token is set" }
                    "It is kept in its own file on the server, not in the settings."
                }
                button { class: "btn", onclick: move |_| on_sign_in.call(()), "Use another account" }
            }
        },
        Account::Idle => rsx! {
            div { class: "panel",
                span { "Signing in with Plex gets this server a token without copying one. plex.tv is asked once, now; the server reads Plex on your network after that." }
                button { class: "btn-dialog btn-dialog-primary", onclick: move |_| on_sign_in.call(()), "Sign in with Plex" }
            }
        },
    }
}

/// Step 4: Plex's folders mapped to where the files are here, with the
/// server's suggestions, each change checked again on a few files.
#[component]
pub fn PathsForm(mode: Mode, on_done: EventHandler<()>) -> Element {
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
        for g in [generation()] {
            PathsFields {
                key: "{g}",
                settings: loaded.clone(),
                mode,
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

/// The saved `plex.path_mapping`.
fn saved_mappings(settings: &Settings) -> Vec<PathMappingEntry> {
    entry(settings, "plex.path_mapping")
        .and_then(|e| e.value.clone())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

async fn check_paths(mappings: Vec<PathMappingEntry>) -> Result<Vec<LibraryPathCheck>, ApiError> {
    let client = rpc::client().await?;
    client
        .plex_path_try(mappings, None, None)
        .await
        .map_err(rpc::error)
}

#[component]
fn PathsFields(
    settings: Settings,
    mode: Mode,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let saved = saved_mappings(&settings);
    let is_locked = locked(&settings, "plex.path_mapping");
    let mut rows = use_signal(Vec::<PathRow>::new);
    let mut checks = use_signal(|| None::<Result<Vec<LibraryPathCheck>, ApiError>>);
    let mut extra = use_signal(Vec::<PathMappingEntry>::new);
    let mut checking = use_signal(|| true);
    let mut round = use_signal(|| 0u32);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    {
        let saved = saved.clone();
        use_hook(move || {
            spawn(async move {
                let result = check_paths(saved.clone()).await;
                // The saved mappings no folder shows stay, as extra rows:
                // Continue saves only what the page has (P3-18). All of
                // them when the check failed and no folder is known.
                let found = result.as_deref().unwrap_or_default();
                extra.set(setup::unattached(found, &saved));
                if let Ok(found) = &result {
                    rows.set(setup::rows(found, &saved));
                    // With the suggestions in, check again: the statuses
                    // are those of what the page shows.
                    if rows.peek().iter().any(|r| r.suggested) {
                        let shown = setup::mappings(&rows.peek(), &extra.peek());
                        checks.set(Some(check_paths(shown).await));
                        checking.set(false);
                        return;
                    }
                }
                checks.set(Some(result));
                checking.set(false);
            });
        });
    }
    let mut recheck = move || {
        round += 1;
        let n = round();
        checking.set(true);
        spawn(async move {
            sleep(600).await;
            if round() != n {
                return;
            }
            let result = check_paths(setup::mappings(&rows.peek(), &extra.peek())).await;
            if round() == n {
                checks.set(Some(result));
                checking.set(false);
            }
        });
    };
    let s = settings.clone();
    let on_save = move |_| {
        let s = s.clone();
        let mappings = setup::mappings(&rows(), &extra());
        busy.set(true);
        spawn(async move {
            let value = (!mappings.is_empty()).then(|| json!(mappings));
            match save(&s, vec![("plex.path_mapping", value)]).await {
                Ok(()) => on_done.call(()),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let results: Vec<LibraryPathCheck> = checks().and_then(Result::ok).unwrap_or_default();
    let missing: Vec<(String, u64, u64, String)> = results
        .iter()
        .filter_map(|c| match setup::sample(c) {
            Sample::Missing { missing, of } => Some((
                c.library.clone(),
                missing,
                of,
                c.failures.first().cloned().unwrap_or_default(),
            )),
            _ => None,
        })
        .collect();
    let blocked = (mode == Mode::Wizard && checking()).then(|| "Still checking".to_owned());
    rsx! {
        if mode == Mode::Wizard {
            div { class: "flex flex-col gap-1.5",
                h1 { "Check the paths" }
                p { class: "lede",
                    "Plex lists files by its own paths. Where those don't exist on this machine, map them to where the files are here. Each change is checked again straight away."
                }
            }
        }
        if let Some(Err(e)) = checks() {
            Alert { text: e.explain() }
        }
        if is_locked {
            Note { settings: settings.clone(), setting: "plex.path_mapping", on_changed: move |_| on_reload.call(()) }
        }
        div {
            div { class: "path-head",
                span { "Library" }
                span { "Plex path" }
                span {}
                span { "On this server" }
                span { {crate::logic::count(5, "sample file")} }
            }
            for (i, row) in rows().into_iter().enumerate() {
                div { key: "{row.library}{row.plex}", class: "path-row",
                    b { "{row.library}" }
                    span { class: "plex-path", "{row.plex}" }
                    span { class: "arrow text-ctp-overlay0", "→" }
                    span { class: "path-local",
                        input {
                            class: if row.suggested { "suggested" } else { "" },
                            value: "{row.local}",
                            disabled: is_locked,
                            aria_label: "Where {row.plex} is on this server",
                            oninput: move |e| {
                                rows.with_mut(|r| {
                                    r[i].local = e.value();
                                    r[i].suggested = false;
                                });
                                recheck();
                            },
                        }
                        if row.suggested {
                            span { class: "tag-suggested", "Suggested" }
                        }
                    }
                    SampleState { check: results.iter().find(|c| c.library == row.library).cloned(), checking: checking() }
                }
            }
            if rows().is_empty() && !checking() {
                p { class: "m-0 text-sm text-ctp-subtext0", "Plex lists no libraries." }
            }
            for (i, m) in extra().into_iter().enumerate() {
                div { key: "extra{i}", class: "path-row",
                    span { class: "text-ctp-subtext0", {m.library.clone().unwrap_or_else(|| "Every library".to_owned())} }
                    span { class: "path-local",
                        input {
                            value: "{m.from}",
                            placeholder: "/plex/path",
                            aria_label: "Plex path",
                            oninput: move |e| {
                                extra.with_mut(|x| x[i].from = e.value());
                                recheck();
                            },
                        }
                    }
                    span { class: "arrow text-ctp-overlay0", "→" }
                    span { class: "path-local",
                        input {
                            value: "{m.to}",
                            placeholder: "/path/here",
                            aria_label: "Where it is on this server",
                            oninput: move |e| {
                                extra.with_mut(|x| x[i].to = e.value());
                                recheck();
                            },
                        }
                    }
                    button {
                        class: "btn",
                        aria_label: "Remove this mapping",
                        title: "Remove this mapping",
                        onclick: move |_| {
                            extra.with_mut(|x| {
                                x.remove(i);
                            });
                            recheck();
                        },
                        icons::X {}
                    }
                }
            }
        }
        div { class: "flex flex-wrap items-center justify-between gap-3",
            span { class: "text-xs text-ctp-subtext0 max-w-[520px]",
                "Suggested mappings come from where the sample files were found. The full check of every file is in Server › Media paths."
            }
            if !is_locked {
                button {
                    class: "btn",
                    onclick: move |_| extra.with_mut(|x| x.push(PathMappingEntry { library: None, from: String::new(), to: String::new() })),
                    icons::Plus { size: 14 }
                    "Add a mapping"
                }
            }
        }
        for (library, n, of, example) in missing {
            div { key: "{library}", class: "alert alert-warning", role: "note",
                icons::Warning {}
                span {
                    b { "{library}: {n} of the {of} sample files are missing" }
                    span { class: "mono-value", {crate::logic::missing_path(&example)} }
                    " You can continue and fix this later."
                }
            }
        }
        if let Some(text) = error() {
            Alert { text }
        }
        Footer { mode, step: "paths", busy: busy(), on_save, on_discard: move |_| on_reload.call(()), blocked }
    }
}

/// A library's sample, at the end of its row.
#[component]
fn SampleState(check: Option<LibraryPathCheck>, checking: bool) -> Element {
    if checking || check.is_none() {
        return rsx! {
            span { class: "flex items-center gap-1.5 text-[13px] font-semibold text-ctp-blue", role: "status",
                span { class: "flex animate-spin", icons::Loader { size: 14 } }
                "Checking…"
            }
        };
    }
    match check.as_ref().map(setup::sample) {
        Some(Sample::Ok { of }) => rsx! {
            span { class: "state state-ok flex items-center gap-1.5 text-[13px]", icons::CircleCheck {} "OK · {of} of {of}" }
        },
        Some(Sample::Missing { missing, of }) => rsx! {
            span { class: "state state-warn flex items-center gap-1.5 text-[13px]", icons::Warning {} "{missing} missing of {of}" }
        },
        _ => rsx! { span { class: "text-[13px] text-ctp-subtext0", "No files" } },
    }
}

/// Step 5: the encoders found, which to use, the limits, and whether
/// picture subtitles are offered as text, with the OCR languages.
#[component]
pub fn PlaybackForm(mode: Mode, on_done: EventHandler<()>) -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let status = use_resource(load_status);
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    rsx! {
        for g in [generation()] {
            PlaybackFields {
                key: "{g}",
                settings: loaded.clone(),
                status: status.read().clone().and_then(Result::ok),
                mode,
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

#[component]
fn PlaybackFields(
    settings: Settings,
    status: Option<ServerStatus>,
    mode: Mode,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let mut hardware =
        use_signal(|| Hardware::from_setting(&text(&settings, "transcode.hardware")));
    let mut order =
        use_signal(
            || match Hardware::from_setting(&text(&settings, "transcode.hardware")) {
                Hardware::Ordered(list) => list,
                _ => "nvenc, vaapi".to_owned(),
            },
        );
    let jobs = use_signal(|| text(&settings, "transcode.max_jobs"));
    let scratch = use_signal(|| {
        text(&settings, "transcode.scratch_limit_mb")
            .parse::<i64>()
            .map(setup::gib)
            .unwrap_or_default()
    });
    let ahead = use_signal(|| text(&settings, "transcode.read_ahead_secs"));
    let workers = use_signal(|| text(&settings, "previews.workers"));
    let mut ocr = use_signal(|| flag(&settings, "subtitles.ocr"));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let s = settings.clone();
    let on_save = move |_| {
        let s = s.clone();
        let changes = (|| -> Result<Vec<(&str, Option<Value>)>, String> {
            let scratch = match scratch().trim() {
                "" => None,
                typed => Some(json!(
                    setup::mib(typed).ok_or("Scratch space is a number of GiB.")?
                )),
            };
            let chosen = match hardware() {
                Hardware::Ordered(_) => Hardware::Ordered(order()),
                other => other,
            };
            Ok(vec![
                ("transcode.hardware", Some(json!(chosen.setting()))),
                ("transcode.max_jobs", number("Encodes at once", &jobs())?),
                ("transcode.scratch_limit_mb", scratch),
                ("transcode.read_ahead_secs", number("Read ahead", &ahead())?),
                (
                    "previews.workers",
                    number("Previews made at once", &workers())?,
                ),
                ("subtitles.ocr", Some(json!(ocr()))),
            ])
        })();
        let changes = match changes {
            Ok(changes) => changes,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        busy.set(true);
        spawn(async move {
            match save(&s, changes).await {
                Ok(()) => on_done.call(()),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let encoders = status
        .as_ref()
        .map(|s| s.transcoding.video_encoders.clone())
        .unwrap_or_default();
    let families = setup::families(&encoders);
    let best = families
        .first()
        .filter(|f| f.name != "Software")
        .map(|f| f.name);
    let languages = status
        .as_ref()
        .map(|s| s.ocr_languages.clone())
        .unwrap_or_default();
    let hw_locked = locked(&settings, "transcode.hardware");
    // Worked out from the server's CPUs, so the server says it.
    let workers_default = match status.as_ref().map(|s| s.preview_workers_default) {
        Some(d) => format!("Default {} ({})", d.count, d.counted.label()),
        None => "Default: one per physical core".to_owned(),
    };
    let choice = |here: bool| if here { "choice choice-here" } else { "choice" };
    rsx! {
        if mode == Mode::Wizard {
            div { class: "flex flex-col gap-1.5",
                h1 { "Playback" }
                p { class: "lede", "Most playback is direct, with no work for the server. These settle what happens when a file has to be converted." }
            }
        }
        div { class: "flex flex-col",
            h2 { class: "mb-2", "Encoders found" }
            div { class: "lines",
                for family in ["VA-API", "NVENC", "Quick Sync", "Software"] {
                    div { key: "{family}",
                        span { class: "flex flex-col gap-0.5",
                            b { "{family}" }
                            span { class: "text-xs text-ctp-subtext0",
                                {match families.iter().find(|f| f.name == family) {
                                    Some(f) if family == "Software" => format!("Always available, uses the CPU · {}", f.codecs.join(", ")),
                                    Some(f) => f.codecs.join(", "),
                                    None if family == "Software" => "Always available, uses the CPU".to_owned(),
                                    None => "Not found on this server".to_owned(),
                                }}
                            }
                        }
                        {match (families.iter().any(|f| f.name == family), family) {
                            (_, "Software") => rsx! { span { class: "flex items-center gap-1.5 text-[13px] text-ctp-subtext0", icons::CircleDashed {} "Fallback" } },
                            (true, _) if Some(family) == best => rsx! { span { class: "state state-ok flex items-center gap-1.5 text-[13px]", icons::CircleCheck {} "Will be used" } },
                            (true, _) => rsx! { span { class: "state state-ok text-[13px]", "Works" } },
                            (false, _) => rsx! { span { class: "flex items-center gap-1.5 text-[13px] text-ctp-subtext0", icons::CircleDashed {} "Not available" } },
                        }}
                    }
                }
            }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Hardware" }
            button {
                class: choice(hardware() == Hardware::Auto),
                disabled: hw_locked,
                onclick: move |_| hardware.set(Hardware::Auto),
                span { class: "radio" }
                span { class: "choice-what",
                    b { "Automatic" }
                    {match best {
                        Some(name) => format!("Uses the best encoder found. Here: {name}."),
                        None => "Uses the best encoder found. Here: software.".to_owned(),
                    }}
                }
            }
            button {
                class: choice(hardware() == Hardware::Software),
                disabled: hw_locked,
                onclick: move |_| hardware.set(Hardware::Software),
                span { class: "radio" }
                span { class: "choice-what", b { "Software only" } "Never use the GPU" }
            }
            div {
                class: choice(matches!(hardware(), Hardware::Ordered(_))),
                onclick: move |_| if !hw_locked { hardware.set(Hardware::Ordered(order())) },
                span { class: "radio" }
                span { class: "choice-what", b { "In this order" } "Try each family in turn, then software" }
                input {
                    value: "{order}",
                    disabled: hw_locked,
                    aria_label: "Encoder families in order",
                    oninput: move |e| {
                        order.set(e.value());
                        hardware.set(Hardware::Ordered(e.value()));
                    },
                }
            }
            Note { settings: settings.clone(), setting: "transcode.hardware", on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Limits" }
            div { class: "limits",
                div { class: "flex flex-col gap-1.5",
                    Limit { label: "Encodes at once", value: jobs, unit: "", disabled: locked(&settings, "transcode.max_jobs") }
                    span { class: "text-xs text-ctp-subtext0", "Default 2" }
                    Note { settings: settings.clone(), setting: "transcode.max_jobs", on_changed: move |_| on_reload.call(()) }
                }
                div { class: "flex flex-col gap-1.5",
                    Limit { label: "Scratch space", value: scratch, unit: "GiB", disabled: locked(&settings, "transcode.scratch_limit_mb") }
                    span { class: "text-xs text-ctp-subtext0", "Default 20 GiB · what all encodes may write together" }
                    Note { settings: settings.clone(), setting: "transcode.scratch_limit_mb", on_changed: move |_| on_reload.call(()) }
                }
                div { class: "flex flex-col gap-1.5",
                    Limit { label: "Read ahead", value: ahead, unit: "s", disabled: locked(&settings, "transcode.read_ahead_secs") }
                    span { class: "text-xs text-ctp-subtext0", "Default 60 s · how far an encode may run ahead of the player" }
                    Note { settings: settings.clone(), setting: "transcode.read_ahead_secs", on_changed: move |_| on_reload.call(()) }
                }
            }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Frame previews" }
            div { class: "limits",
                div { class: "flex flex-col gap-1.5",
                    Limit { label: "Made at once", value: workers, unit: "", disabled: locked(&settings, "previews.workers") }
                    span { class: "text-xs text-ctp-subtext0", "{workers_default} · files made ahead together, only while nothing is converted" }
                    Note { settings: settings.clone(), setting: "previews.workers", on_changed: move |_| on_reload.call(()) }
                }
            }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Subtitles" }
            div { class: "flex items-start gap-3",
                button {
                    class: "switch mt-0.5",
                    role: "switch",
                    aria_checked: "{ocr()}",
                    aria_label: "Offer picture subtitles as text",
                    disabled: locked(&settings, "subtitles.ocr"),
                    onclick: move |_| ocr.toggle(),
                }
                span { class: "choice-what",
                    b { "Offer picture subtitles as text (OCR)" }
                    "Picture subtitles (PGS, VobSub, DVB) can be read into text, so any device can show them styled and sized. Each user picks what they see."
                }
            }
            Note { settings: settings.clone(), setting: "subtitles.ocr", on_changed: move |_| on_reload.call(()) }
            if ocr() && !languages.is_empty() {
                div { class: "chips",
                    for code in languages.clone() {
                        span { key: "{code}", title: "{code}", {setup::language_name(&code)} }
                    }
                }
            }
            span { class: "text-xs text-ctp-subtext0", {setup::ocr_note(ocr(), !languages.is_empty())} }
        }
        if let Some(text) = error() {
            Alert { text }
        }
        Footer { mode, step: "playback", busy: busy(), on_save, on_discard: move |_| on_reload.call(()) }
    }
}

/// A number with its unit beside it.
#[component]
fn Limit(label: String, value: Signal<String>, unit: String, disabled: bool) -> Element {
    let mut value = value;
    rsx! {
        label { class: "field",
            "{label}"
            span { class: "limit-input",
                input {
                    inputmode: "numeric",
                    class: if disabled { "locked" } else { "" },
                    value: "{value}",
                    disabled,
                    oninput: move |e| value.set(e.value()),
                }
                if !unit.is_empty() {
                    span { "{unit}" }
                }
            }
        }
    }
}

/// Step 6: the web UI's public address, the proxies in front of it, and
/// where it listens.
#[component]
pub fn AccessForm(mode: Mode, on_done: EventHandler<()>) -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let status = use_resource(load_status);
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    rsx! {
        for g in [generation()] {
            AccessFields {
                key: "{g}",
                settings: loaded.clone(),
                status: status.read().clone().and_then(Result::ok),
                mode,
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

#[component]
fn AccessFields(
    settings: Settings,
    status: Option<ServerStatus>,
    mode: Mode,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let public_url = use_signal(|| text(&settings, "admin.public_url"));
    let proxies = use_signal(|| text(&settings, "admin.trusted_proxies"));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let s = settings.clone();
    // `admin.*`: from an old session, "Confirm it's you" first.
    let confirming = use_signal(|| None::<String>);
    let on_save = use_callback(move |()| {
        let s = s.clone();
        busy.set(true);
        spawn(async move {
            let changes = vec![
                ("admin.public_url", typed(&public_url())),
                ("admin.trusted_proxies", typed(&proxies())),
            ];
            match save(&s, changes).await {
                Ok(()) => on_done.call(()),
                Err(e)
                    if asks_to_confirm(&e, confirming, "changing how people reach the web UI") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let url = public_url();
    let https = url.trim().starts_with("https://");
    let host = crate::logic::host_port(url.trim())
        .split(':')
        .next()
        .unwrap_or_default()
        .to_owned();
    let named =
        !host.is_empty() && host.parse::<std::net::IpAddr>().is_err() && !host.starts_with('[');
    let here = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .is_some_and(|origin| {
            !url.trim().is_empty() && url.trim().trim_end_matches('/').starts_with(&origin)
        });
    let listen = text(&settings, "admin.listen");
    let answers = status.map(|s| s.web_urls).unwrap_or_default();
    rsx! {
        if mode == Mode::Wizard {
            div { class: "flex flex-col gap-1.5",
                h1 { "How people reach the web UI" }
                p { class: "lede", "The address people type to open this page. It's also the address passkeys and sign-in providers are tied to." }
            }
        }
        div { class: "flex flex-col gap-2",
            Field { label: "Public address", value: public_url, placeholder: "https://media.example.org", tall: true, disabled: locked(&settings, "admin.public_url") }
            if !url.trim().is_empty() {
                div { class: "checks",
                    span { class: if here { "yes" } else { "no" },
                        if here { icons::CircleCheck {} } else { icons::CircleDashed {} }
                        if here { "This page is open at this address" } else { "Not the address this page is open at" }
                    }
                    span { class: if https { "yes" } else { "no" },
                        if https { icons::CircleCheck {} } else { icons::CircleDashed {} }
                        "HTTPS"
                    }
                    span { class: if https && named { "yes" } else { "no" },
                        if https && named { icons::CircleCheck {} } else { icons::CircleDashed {} }
                        if https && named { "Passkeys work here" } else { "Passkeys need HTTPS and a name" }
                    }
                }
            }
            Note { settings: settings.clone(), setting: "admin.public_url", on_changed: move |_| on_reload.call(()) }
        }
        div { class: "alert alert-info", role: "note",
            icons::Info {}
            span {
                b { "Why HTTPS matters" }
                "Browsers only allow passkeys, sign-in providers and secure cookies over HTTPS (or at localhost). Put a reverse proxy such as Caddy or nginx in front of the server and give its address here. Changing it later means passkeys have to be added again."
            }
        }
        div { class: "flex flex-col gap-1.5",
            Field { label: "Trusted proxies", value: proxies, placeholder: "127.0.0.1, 192.168.1.2", disabled: locked(&settings, "admin.trusted_proxies") }
            span { class: "text-xs text-ctp-subtext0", "Addresses of your reverse proxy, so the server believes the client address it forwards." }
            Note { settings: settings.clone(), setting: "admin.trusted_proxies", on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-1.5",
            div { class: "flex items-baseline justify-between gap-3",
                h2 { "Listen address" }
                span { class: "note note-restart", icons::RotateCcw { size: 13 } "Applies after a restart" }
            }
            span { class: "mono-value", "{listen}" }
            if !answers.is_empty() {
                span { class: "text-xs text-ctp-subtext0", {format!("The web UI answers at {}", answers.join(" · "))} }
            }
            span { class: "text-xs text-ctp-subtext0", "Change it with " span { class: "mono-value text-xs", "spoolframe-server config set admin.listen …" } " or in the server's config file." }
        }
        div { class: "later", "Remote access, outside your home network, joins this step in a later version." }
        if let Some(text) = error() {
            Alert { text }
        }
        Footer { mode, step: "access", busy: busy(), on_save, on_discard: move |_| on_reload.call(()) }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
    }
}

/// Settings' Security tab: insecure cookies.
#[component]
pub fn SecurityForm() -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let mut error = use_signal(|| None::<String>);
    // `admin.*`: from an old session, "Confirm it's you" first, then the
    // same change again. `on` is whether the cookies are insecure now.
    let confirming = use_signal(|| None::<String>);
    let toggle = use_callback(move |on: bool| {
        spawn(async move {
            let changed = async {
                let client = rpc::client().await?;
                if on {
                    client
                        .setting_unset("admin.insecure_cookies".into())
                        .await
                        .map_err(rpc::error)
                } else {
                    client
                        .setting_set("admin.insecure_cookies".into(), json!(true))
                        .await
                        .map_err(rpc::error)
                }
            };
            match changed.await {
                Ok(_) => {
                    // The banner reads it too.
                    if let Some(window) = web_sys::window() {
                        let _ = window.location().reload();
                    }
                }
                Err(e)
                    if asks_to_confirm(&e, confirming, "changing how sign-in cookies are sent") => {
                }
                Err(e) => error.set(Some(e.explain())),
            }
            generation += 1;
        });
    });
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    let on = flag(&loaded, "admin.insecure_cookies");
    rsx! {
        div { class: "flex items-start gap-3",
            button {
                class: "switch mt-0.5",
                role: "switch",
                aria_checked: "{on}",
                aria_label: "Allow insecure cookies",
                disabled: locked(&loaded, "admin.insecure_cookies"),
                onclick: move |_| toggle.call(on),
            }
            span { class: "choice-what",
                b { "Allow insecure cookies" }
                "Sends the session cookie without its Secure flag, so signing in over plain HTTP on your network works. The cookie then crosses your network unencrypted. Passkeys still need HTTPS or localhost. Not allowed when the public URL is HTTPS."
            }
        }
        Note { settings: loaded.clone(), setting: "admin.insecure_cookies", on_changed: move |_| generation += 1 }
        if let Some(text) = error() {
            Alert { text }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| toggle.call(on) }
    }
}

/// Step 7: what was set up, each with a way to change it, and the end of
/// the wizard.
#[component]
pub fn DoneCard() -> Element {
    let nav = navigator();
    let settings = use_resource(load_settings);
    let status = use_resource(load_status);
    let me = use_resource(|| async {
        let client = rpc::client().await?;
        client.who_am_i().await.map_err(rpc::error)
    });
    let plex = use_resource(|| async {
        let client = rpc::client().await?;
        client.plex_status().await.map_err(rpc::error)
    });
    // The Paths step's check again, on the saved mappings: a library with
    // files this server cannot read is named here (P3-18).
    let checked = use_resource(|| async {
        let settings = load_settings().await?;
        check_paths(saved_mappings(&settings)).await
    });
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let finish = move |_| {
        busy.set(true);
        spawn(async move {
            let finished = async {
                let client = rpc::client().await?;
                client.setup_finish().await.map_err(rpc::error)
            };
            match finished.await {
                Ok(_) => {
                    nav.replace(Route::from(Landing::Admin));
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let s = settings.read().clone().and_then(Result::ok);
    let name = status
        .read()
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "Spoolframe".into());
    let fingerprint = status
        .read()
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .and_then(|s| s.fingerprint.clone())
        .unwrap_or_default();
    let admin = me.read().as_ref().and_then(|m| m.as_ref().ok()).map(|m| {
        let how = match (m.has_passkey, m.has_password) {
            (true, true) => "passkey and password",
            (true, false) => "passkey",
            _ => "password",
        };
        format!("{} · {how}", m.username)
    });
    let plex_line = match plex.read().as_ref() {
        Some(Ok(p)) if p.reachable => format!(
            "{} · {}",
            p.server_name.clone().unwrap_or_else(|| "Plex".into()),
            super::count_of(p.libraries.len() as u64, "library", "libraries")
        ),
        Some(Ok(_)) => "Not reachable now".into(),
        Some(Err(_)) => "Not set up".into(),
        None => "…".into(),
    };
    let paths_line = s
        .as_ref()
        .map(|s| match saved_mappings(s).len() {
            0 => "The same paths as Plex's".to_owned(),
            n => crate::logic::count(n as u64, "mapping"),
        })
        .unwrap_or_default();
    let missing = match &*checked.read() {
        Some(Ok(checks)) => setup::missing_summary(&setup::missing_files(checks)),
        _ => None,
    };
    let encoders = status
        .read()
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .map(|s| s.transcoding.video_encoders.clone());
    let playback_line = s
        .as_ref()
        .map(|s| {
            setup::playback_summary(
                &Hardware::from_setting(&text(s, "transcode.hardware")),
                encoders.as_deref(),
                text(s, "transcode.max_jobs").parse().unwrap_or(2),
            )
        })
        .unwrap_or_default();
    let access_line = s
        .as_ref()
        .map(|s| match text(s, "admin.public_url") {
            url if url.is_empty() => "No public address: this network only".to_owned(),
            url => url,
        })
        .unwrap_or_default();
    let row = |label: &'static str, value: String, step: &'static str| {
        rsx! {
            div { key: "{label}", class: "done-line",
                span { class: "done-label", "{label}" }
                span { class: "done-value", "{value}" }
                Link { class: "done-change", to: Route::Wizard { step: step.to_owned() }, "Change" }
            }
        }
    };
    rsx! {
        div { class: "flex flex-col items-center gap-2 text-center",
            span { class: "flex text-ctp-green", icons::CircleCheck { size: 48 } }
            h1 { "{name} is ready" }
            p { class: "lede", "Apps on your network can sign in now." }
        }
        div { class: "lines",
            div { key: "Admin", class: "done-line",
                span { class: "done-label", "Admin" }
                span { class: "done-value", {admin.unwrap_or_default()} }
                Link { class: "done-change", to: Route::Profile {}, "Change" }
            }
            div { key: "Server", class: "done-line",
                span { class: "done-label", "Server" }
                span { class: "done-value",
                    "{name}"
                    if !fingerprint.is_empty() {
                        " · "
                        span { class: "mono-value whitespace-nowrap", "{fingerprint}" }
                    }
                }
                Link { class: "done-change", to: Route::Wizard { step: "server".to_owned() }, "Change" }
            }
            {row("Plex", plex_line, "plex")}
            div { key: "Paths", class: "done-line",
                span { class: "done-label", "Paths" }
                span { class: "done-value", "{paths_line}" }
                if let Some(text) = missing {
                    span { class: "done-warn state state-warn",
                        span { class: "flex", icons::Warning { size: 15 } }
                        "{text}"
                    }
                }
                Link { class: "done-change", to: Route::Wizard { step: "paths".to_owned() }, "Change" }
            }
            {row("Playback", playback_line, "playback")}
            {row("Access", access_line, "access")}
        }
        p { class: "m-0 text-center text-xs text-ctp-subtext0", "Every one of these is also under Server › Settings." }
        if let Some(text) = error() {
            Alert { text }
        }
        div { class: "flex justify-center",
            button { class: "btn-dialog btn-dialog-primary h-11", disabled: busy(), onclick: finish,
                icons::ArrowRight {}
                "Open the admin UI"
            }
        }
    }
}

/// A wizard left half-way, opened again: where it stopped.
#[component]
pub fn Resume(at: Step, done: Vec<String>) -> Element {
    let (n, _) = setup::step(at.id).unwrap_or((1, at));
    rsx! {
        section { class: "wizard-card max-w-[460px]",
            div { class: "flex items-center gap-3",
                span { class: "flex text-ctp-teal", icons::History {} }
                h1 { class: "text-[22px]", "Welcome back" }
            }
            p { class: "lede", "You stopped at step {n}, {at.title}. {setup::saved_before(at)}" }
            Steps { current: at.id.to_owned(), done }
            Link { class: "btn-primary no-underline", to: Route::Wizard { step: at.id.to_owned() },
                icons::ArrowRight {}
                "Continue at {at.title}"
            }
        }
    }
}
