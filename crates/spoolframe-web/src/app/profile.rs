//! "My profile" (P3-03), as the design's WebProfile and WebProfilePhone
//! boards lay it out for a consumer (one column, its own page) and
//! WebAdminProfile for an admin (two columns, in the admin shell): the
//! signed-in user's own account, over the `me.*` methods. The account (display name, password), the profile PIN and
//! where it is asked for, devices and sessions, playback preferences,
//! the sign-in methods (password, passkeys from P3-05), and the Plex
//! account (P3-12). A sensitive change made long after signing in asks
//! the user to confirm it is them first, then goes on.

use dioxus::prelude::*;
use serde_json::Value;
use spoolframe_admin_api::{
    DeviceEntry, MeApiClient, PasskeyEntry, PasskeyHost, PasswordChange, Role, SessionEntry,
    SsoLink, TrackOverrideEntry, TrackPreferences, UserInfo, error_code,
};

use super::{Alert, Field, Notice, ShownName, http, icons, now, rpc, webauthn};
use crate::logic::{ApiError, ago, avatar_class, client_kind, date, page_serves, passkey_title};
use crate::preferences::{
    self, SDH, SUBTITLE_MODES, audio_choices, avoids_extras, index_of, override_line,
    subtitle_choices,
};
use crate::sso::Tone;

#[component]
pub fn Profile() -> Element {
    let user = use_context::<UserInfo>();
    let ShownName(shown) = use_context::<ShownName>();
    // Read at sign-in, and kept here: adding a first password changes it.
    let has_password = use_signal(|| user.has_password);
    // What a sign-in provider's callback sent the browser here to say.
    let said = use_hook(|| {
        http::sso_outcome("/profile")
            .and_then(|(what, provider)| crate::sso::profile_message(&what, provider.as_deref()))
    });
    if user.role == Role::Admin {
        // In the admin shell (WebAdminProfile): two columns on a wide
        // screen, one on a phone in `order`.
        return rsx! {
            div { class: "flex flex-col gap-[18px]",
                h1 { class: "page-title hidden md:block", "My profile" }
                div { class: "profile-grid",
                    div { class: "profile-col",
                        div { style: "order:1", Account { user: user.clone(), has_password } }
                        div { style: "order:3", PinCard { has_password, has_pin: user.has_pin } }
                        div { style: "order:5", Playback {} }
                    }
                    div { class: "profile-col",
                        div { style: "order:2", Passkeys { has_password, said: said.clone() } }
                        div { style: "order:4", Sessions {} }
                        div { style: "order:6", super::plex::PlexAccountCard {} }
                    }
                }
            }
        };
    }
    // A consumer's one page (WebProfile, WebProfilePhone): one column.
    let avatar = avatar_class(&user.username, false);
    let initial = shown()
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    rsx! {
        div { class: "profile-column",
            div { class: "profile-head",
                span { class: "{avatar} avatar-xl", "{initial}" }
                div { class: "flex min-w-0 flex-col",
                    h1 { "My profile" }
                    span { class: "truncate text-sm text-ctp-subtext0", "{shown} · {user.username}" }
                }
            }
            Account { user: user.clone(), has_password }
            Passkeys { has_password, said }
            super::plex::PlexAccountCard {}
            Sessions {}
            PinCard { has_password, has_pin: user.has_pin }
            Playback {}
        }
    }
}

/// Who the user is: the display name, which they change here, the
/// username, and the password, or adding one where there is none (set up
/// with a passkey).
#[component]
fn Account(user: UserInfo, has_password: Signal<bool>) -> Element {
    let mut has_password = has_password;
    let mut changing = use_signal(|| false);
    let mut notice = use_signal(|| None::<String>);
    let ShownName(mut shown) = use_context::<ShownName>();
    let name = use_signal(|| user.display_name.clone().unwrap_or_default());
    let mut saved = use_signal(|| user.display_name.clone().unwrap_or_default());
    let mut name_said = use_signal(|| None::<Result<String, String>>);
    let mut busy = use_signal(|| false);
    let save_name = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        spawn(async move {
            let set = async {
                let client = rpc::client().await?;
                let typed = name();
                let chosen = Some(typed).filter(|n| !n.trim().is_empty());
                client.set_display_name(chosen).await.map_err(rpc::error)
            };
            match set.await {
                Ok(me) => {
                    saved.set(me.display_name.clone().unwrap_or_default());
                    shown.set(me.display_name.unwrap_or(me.username));
                    name_said.set(Some(Ok("Saved. Your devices show it from now on.".into())));
                }
                Err(e) => name_said.set(Some(Err(e.explain()))),
            }
            busy.set(false);
        });
    };
    let changed_at = user.password_changed_at;
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::User {}
                h2 { "Account" }
            }
            div { class: "card-body",
                form { class: "flex items-end gap-2", onsubmit: save_name,
                    div { class: "min-w-0 flex-grow",
                        Field { label: "Display name", value: name, autocomplete: "nickname", placeholder: "{user.username}" }
                    }
                    if name() != saved() {
                        button { class: "btn h-10", r#type: "submit", disabled: busy(), "Save" }
                    }
                }
                match name_said() {
                    Some(Ok(text)) => rsx! { p { class: "m-0 text-xs text-ctp-green", role: "status", "{text}" } },
                    Some(Err(text)) => rsx! { Alert { text } },
                    None => rsx! {},
                }
                div { class: "row",
                    span { class: "row-what", b { "Username" } }
                    span { class: "row-do", span { class: "text-[13px] text-ctp-subtext0", "{user.username}" } }
                }
                div { class: "row",
                    span { class: "row-what",
                        b { "Password" }
                        if let Some(text) = notice() {
                            span { class: "row-sub text-ctp-green", role: "status", "{text}" }
                        } else if let (true, Some(at)) = (has_password(), changed_at) {
                            span { class: "row-sub", "Changed {ago(now(), at)}" }
                        } else if !has_password() {
                            span { class: "row-sub", "Not set" }
                        }
                    }
                    span { class: "row-do",
                        if !changing() {
                            button { class: "btn", onclick: move |_| changing.set(true),
                                if has_password() { "Change password" } else { "Add a password" }
                            }
                        }
                    }
                }
                if changing() {
                    ChangePassword {
                        first: !has_password(),
                        on_done: move |changed: bool| {
                            changing.set(false);
                            if changed {
                                let first = !has_password();
                                has_password.set(true);
                                notice.set(Some(if first { "Added just now" } else { "Changed just now" }.into()));
                            }
                        },
                    }
                }
            }
        }
    }
}

/// The password, changed with the current one. `first`: the user has
/// none, and a recent sign-in stands in for it, confirmed with a passkey
/// if it is not recent.
#[component]
fn ChangePassword(first: bool, on_done: EventHandler<bool>) -> Element {
    let current = use_signal(String::new);
    let new = use_signal(String::new);
    let again = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut confirming = use_signal(|| false);
    let save = use_callback(move |()| {
        busy.set(true);
        spawn(async move {
            let changed = async {
                let client = rpc::client().await?;
                let change = PasswordChange {
                    current_password: current(),
                    new_password: new(),
                };
                client.set_password(change).await.map_err(rpc::error)
            };
            match changed.await {
                Ok(()) => on_done.call(true),
                Err(e) if first && e.code == error_code::RECENT_SIGN_IN_REQUIRED => {
                    confirming.set(true);
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let submit = move |e: FormEvent| {
        e.prevent_default();
        if new() != again() {
            error.set(Some("The new passwords are not the same.".into()));
            return;
        }
        save.call(());
    };
    rsx! {
        form { class: "flex flex-col gap-3", onsubmit: submit,
            if !first {
                Field { label: "Current password", value: current, kind: "password", autocomplete: "current-password" }
            }
            Field { label: "New password", value: new, kind: "password", autocomplete: "new-password" }
            Field { label: "New password again", value: again, kind: "password", autocomplete: "new-password" }
            span { class: "hint", "Every other browser and device is signed out. This one stays signed in." }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "flex justify-end gap-2.5",
                button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(false), "Cancel" }
                button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(),
                    if first { "Add password" } else { "Change password" }
                }
            }
        }
        if confirming() {
            ConfirmDialog {
                with_password: false,
                about: "adding a password",
                on_confirmed: move |_| {
                    confirming.set(false);
                    save.call(());
                },
                on_cancel: move |_| confirming.set(false),
            }
        }
    }
}

/// A change that needs a recent sign-in, kept while the user confirms it
/// is them.
#[derive(Debug, Clone, PartialEq)]
enum Sensitive {
    Add,
    Remove(PasskeyEntry),
    /// Link an account at the sign-in provider called this (P3-04).
    Link(String),
    Unlink(SsoLink),
}

async fn passkeys() -> Result<Vec<PasskeyEntry>, ApiError> {
    let client = rpc::client().await?;
    client.passkeys().await.map_err(rpc::error)
}

async fn host() -> Result<PasskeyHost, ApiError> {
    let client = rpc::client().await?;
    client.passkey_host().await.map_err(rpc::error)
}

/// The host of the page, as the browser has it.
fn page_host() -> String {
    web_sys::window()
        .and_then(|window| window.location().hostname().ok())
        .unwrap_or_default()
}

/// Why this page cannot add a passkey, if it cannot.
fn cannot_add(host: &Result<PasskeyHost, ApiError>) -> Option<String> {
    match host {
        // The server's own reason: plain HTTP, or an address for a host.
        Err(e) => Some(e.explain()),
        Ok(_) if !webauthn::supported() => Some(
            "This browser cannot use passkeys here: they need HTTPS (or localhost) and a browser that has them."
                .into(),
        ),
        Ok(host) if !page_serves(&page_host(), &host.rp_id) => Some(format!(
            "Passkeys are bound to {}. Open the web UI at {} to add one.",
            host.rp_id, host.origin
        )),
        Ok(_) => None,
    }
}

async fn sso_links() -> Result<Vec<SsoLink>, ApiError> {
    let client = rpc::client().await?;
    client.sso_links().await.map_err(rpc::error)
}

/// What a link is called: its provider's name, or its issuer when no
/// provider has it any more.
fn provider_name(link: &SsoLink) -> String {
    link.provider.clone().unwrap_or_else(|| link.issuer.clone())
}

/// The sign-in methods card: the password, the passkeys, and the
/// sign-in providers (P3-04): each configured one linked or not, and any
/// link whose provider is gone.
#[component]
fn Passkeys(has_password: Signal<bool>, said: Option<(Tone, String)>) -> Element {
    let mut list = use_resource(passkeys);
    let host = use_resource(host);
    let mut links = use_resource(sso_links);
    let site = use_resource(http::site);
    let (said_good, said_bad) = match said {
        Some((Tone::Good, text)) => (Some(text), None),
        Some((Tone::Bad, text)) => (None, Some(text)),
        None => (None, None),
    };
    let mut notice = use_signal(|| said_good);
    let mut error = use_signal(|| said_bad);
    // The registration the server began, while the dialog naming the new
    // passkey is open.
    let mut adding = use_signal(|| None::<Value>);
    let mut confirming = use_signal(|| None::<Sensitive>);
    // Try `change`; if the session is too old for it, ask the user to
    // confirm it is them and try again.
    let attempt = use_callback(move |change: Sensitive| {
        spawn(async move {
            let tried = async {
                let client = rpc::client().await?;
                match &change {
                    Sensitive::Add => {
                        let options = client.passkey_add_begin().await.map_err(rpc::error)?;
                        adding.set(Some(options));
                    }
                    Sensitive::Remove(passkey) => {
                        client
                            .passkey_remove(passkey.id.clone())
                            .await
                            .map_err(rpc::error)?;
                        notice.set(Some(format!("{} removed.", passkey.name)));
                        list.restart();
                    }
                    // To the provider, which sends the browser back here.
                    Sensitive::Link(provider) => {
                        http::go(&http::sso_begin(provider, "link", None).await?)
                    }
                    Sensitive::Unlink(link) => {
                        client
                            .sso_unlink(link.issuer.clone())
                            .await
                            .map_err(rpc::error)?;
                        notice.set(Some(format!("{} unlinked.", provider_name(link))));
                        links.restart();
                    }
                }
                Ok::<(), ApiError>(())
            };
            match tried.await {
                Ok(()) => {
                    confirming.set(None);
                    error.set(None);
                }
                Err(e) if e.code == error_code::RECENT_SIGN_IN_REQUIRED => {
                    confirming.set(Some(change));
                }
                Err(e) => {
                    confirming.set(None);
                    error.set(Some(e.explain()));
                }
            }
        });
    });
    let current = list.read().clone();
    let blocked = host.read().as_ref().map(cannot_add);
    let has_passkeys = matches!(&current, Some(Ok(p)) if !p.is_empty());
    let has_links = matches!(&*links.read(), Some(Ok(l)) if !l.is_empty());
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::Key { size: 18 }
                h2 { "Sign-in methods" }
            }
            div { class: "card-body",
                div { class: "row",
                    span { class: "row-what", span { icons::KeyRound {} b { "Password" } } }
                    span { class: "row-do",
                        if has_password() {
                            span { class: "text-xs text-ctp-green", "Set" }
                        } else {
                            span { class: "text-xs text-ctp-subtext0", "Not set" }
                        }
                    }
                }
                if !has_password() {
                    match only_ways(has_passkeys, has_links) {
                        Some(ways) => rsx! {
                            Notice {
                                b { "{ways} " }
                                "If you lose them, getting back in needs an admin, or for the only admin, shell access to the server. Add a password as well, under Account."
                            }
                        },
                        None => rsx! {},
                    }
                }
                if let Some(text) = notice() {
                    p { class: "m-0 text-sm text-ctp-green", role: "status", "{text}" }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "row-label", "Passkeys" }
                match current {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(passkeys)) if passkeys.is_empty() => rsx! {
                        p { class: "m-0 text-sm text-ctp-subtext0", "No passkeys yet." }
                    },
                    Some(Ok(passkeys)) => rsx! {
                        for passkey in passkeys {
                            PasskeyRow {
                                key: "{passkey.id}",
                                passkey,
                                on_remove: move |passkey| attempt.call(Sensitive::Remove(passkey)),
                                on_renamed: move |_| list.restart(),
                            }
                        }
                    },
                }
                match blocked {
                    None | Some(None) => rsx! {
                        button {
                            class: "btn btn-grey self-start",
                            onclick: move |_| {
                                notice.set(None);
                                attempt.call(Sensitive::Add);
                            },
                            icons::Plus {}
                            "Add a passkey"
                        }
                    },
                    Some(Some(why)) => rsx! {
                        p { class: "m-0 text-xs text-ctp-yellow", "{why}" }
                    },
                }
                {
                    let providers = site.read().as_ref().and_then(|s| s.as_ref().ok()).map(|s| s.sso.clone()).unwrap_or_default();
                    let linked = links.read().as_ref().and_then(|l| l.as_ref().ok()).cloned().unwrap_or_default();
                    let gone: Vec<SsoLink> = linked.iter().filter(|l| l.provider.as_ref().is_none_or(|p| !providers.contains(p))).cloned().collect();
                    rsx! {
                        if !providers.is_empty() || !gone.is_empty() {
                            div { class: "row-label", "Sign-in providers" }
                        }
                        for provider in providers.iter().cloned() {
                            {
                                let link = linked.iter().find(|l| l.provider.as_deref() == Some(provider.as_str())).cloned();
                                rsx! {
                                    div { class: "row", key: "{provider}",
                                        span { class: "row-what",
                                            span { class: "break-words", icons::ShieldCheck { size: 15 } b { "{provider}" } }
                                            match &link {
                                                Some(l) => rsx! { span { class: "row-sub", "{link_line(l)}" } },
                                                None => rsx! { span { class: "row-sub", "Not linked" } },
                                            }
                                        }
                                        span { class: "row-do",
                                            match link {
                                                Some(l) => rsx! {
                                                    button { class: "btn btn-danger", onclick: move |_| attempt.call(Sensitive::Unlink(l.clone())), icons::Unlink { size: 15 } "Unlink" }
                                                },
                                                None => rsx! {
                                                    button {
                                                        class: "btn",
                                                        onclick: {
                                                            let provider = provider.clone();
                                                            move |_| {
                                                                notice.set(None);
                                                                attempt.call(Sensitive::Link(provider.clone()));
                                                            }
                                                        },
                                                        icons::LinkChain {}
                                                        "Link {provider}"
                                                    }
                                                },
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        for l in gone {
                            div { class: "row", key: "{l.issuer}",
                                span { class: "row-what",
                                    span { class: "break-words", icons::ShieldCheck { size: 15 } b { "{provider_name(&l)}" } }
                                    span { class: "row-sub text-ctp-yellow", "This provider is no longer set up on the server: it signs you in nowhere." }
                                }
                                span { class: "row-do",
                                    button { class: "btn btn-danger", onclick: move |_| attempt.call(Sensitive::Unlink(l.clone())), icons::Unlink { size: 15 } "Unlink" }
                                }
                            }
                        }
                    }
                }
                p { class: "m-0 text-xs text-ctp-subtext0", "You can't remove your last sign-in method." }
            }
        }
        if let Some(change) = confirming() {
            ConfirmDialog {
                with_password: has_password(),
                about: "changing your sign-in methods",
                on_confirmed: move |_| attempt.call(change.clone()),
                on_cancel: move |_| confirming.set(None),
            }
        }
        if let Some(options) = adding() {
            AddPasskeyDialog {
                options,
                host: host.read().clone().and_then(Result::ok),
                on_added: move |name: String| {
                    adding.set(None);
                    notice.set(Some(format!("{name} added.")));
                    list.restart();
                },
                on_cancel: move |_| adding.set(None),
            }
        }
    }
}

/// One passkey: its name, where it works and since when, and what to do
/// with it.
#[component]
fn PasskeyRow(
    passkey: PasskeyEntry,
    on_remove: EventHandler<PasskeyEntry>,
    on_renamed: EventHandler<()>,
) -> Element {
    let mut renaming = use_signal(|| false);
    let mut removing = use_signal(|| false);
    let name = use_signal(|| passkey.name.clone());
    let mut error = use_signal(|| None::<String>);
    let id = passkey.id.clone();
    let rename = move |e: FormEvent| {
        e.prevent_default();
        let id = id.clone();
        spawn(async move {
            let renamed = async {
                let client = rpc::client().await?;
                client.passkey_rename(id, name()).await.map_err(rpc::error)
            };
            match renamed.await {
                Ok(()) => {
                    renaming.set(false);
                    error.set(None);
                    on_renamed.call(());
                }
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };
    let used = match passkey.last_used_at {
        Some(at) => format!(" · used {}", ago(now(), at)),
        None => String::new(),
    };
    rsx! {
        div { class: "row",
            span { class: "row-what",
                span { class: "break-words",
                    icons::Fingerprint {}
                    b { {passkey_title(&passkey)} }
                }
                span { class: "row-sub", "{passkey.rp_id} · added {date(passkey.created_at)}{used}" }
                if !passkey.usable {
                    span { class: "row-sub text-ctp-yellow",
                        "Made for {passkey.rp_id}: it does not work at this server's address now."
                    }
                }
            }
            span { class: "row-do",
                if removing() {
                    span { class: "text-[13px]", "Remove it?" }
                    button {
                        class: "btn btn-danger",
                        onclick: {
                            let passkey = passkey.clone();
                            move |_| {
                                removing.set(false);
                                on_remove.call(passkey.clone());
                            }
                        },
                        "Remove"
                    }
                    button { class: "btn", onclick: move |_| removing.set(false), "Keep" }
                } else {
                    button { class: "btn", onclick: move |_| renaming.toggle(), icons::Pencil {} "Rename" }
                    button { class: "btn btn-danger", onclick: move |_| removing.set(true), icons::Trash {} "Remove" }
                }
            }
        }
        if renaming() {
            form { class: "flex items-end gap-2", onsubmit: rename,
                Field { label: "Name", value: name, autocomplete: "off" }
                button { class: "btn", r#type: "submit", "Save" }
            }
            if let Some(text) = error() {
                Alert { text }
            }
        }
    }
}

/// The design's "Web · Confirm it's you": asked when a sensitive change
/// comes from a session whose sign-in is more than 10 minutes old
/// (P3-03). The password, or one of the user's own passkeys; only a
/// passkey `with_password` false, for a user who has none. `about`: what
/// the change is, "changing your sign-in methods".
#[component]
pub(super) fn ConfirmDialog(
    with_password: bool,
    about: String,
    on_confirmed: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // The passkey request, fetched as the dialog opens, so "Use a passkey"
    // opens the prompt from the click itself, as Safari requires.
    let mut request = use_resource(super::passkey_request);
    // The sign-in providers the user has linked, which can confirm too
    // (P3-04): through the provider and back to My profile, which then
    // says to go on.
    let linked = use_resource(|| async {
        let links = sso_links().await.unwrap_or_default();
        let site = http::site().await.unwrap_or_default();
        site.sso
            .into_iter()
            .filter(|p| {
                links
                    .iter()
                    .any(|l| l.provider.as_deref() == Some(p.as_str()))
            })
            .collect::<Vec<String>>()
    });
    let providers = linked.read().clone().unwrap_or_default();
    let with_provider = use_callback(move |provider: String| {
        busy.set(true);
        spawn(async move {
            match http::sso_begin(&provider, "confirm", None).await {
                Ok(url) => http::go(&url),
                Err(e) => {
                    error.set(Some(e.explain()));
                    busy.set(false);
                }
            }
        });
    });
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        spawn(async move {
            let confirmed = async {
                let client = rpc::client().await?;
                client
                    .confirm_password(password())
                    .await
                    .map_err(rpc::error)
            };
            match confirmed.await {
                Ok(()) => on_confirmed.call(()),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let with_passkey = move |_| {
        busy.set(true);
        let opened = super::open_passkey_prompt(request.read().clone().flatten());
        spawn(async move {
            let confirmed = async {
                let (challenge, signed) = super::passkey_signature(opened).await?;
                let client = rpc::client().await.map_err(|e| e.explain())?;
                client
                    .confirm_passkey(challenge.ceremony, signed)
                    .await
                    .map_err(|e| rpc::error(e).explain())
            };
            match confirmed.await {
                Ok(()) => on_confirmed.call(()),
                Err(text) => {
                    error.set(Some(text));
                    // A request is good once: the next try needs another.
                    request.restart();
                }
            }
            busy.set(false);
        });
    };
    rsx! {
        div { class: "scrim",
            form { class: "dialog", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Shield {}
                    h1 { "Confirm it's you" }
                }
                p { class: "m-0 text-sm leading-normal text-ctp-subtext1",
                    "You signed in more than 10 minutes ago. Confirm it's you before {about}."
                }
                if with_password {
                    Field { label: "Password", value: password, kind: "password", autocomplete: "current-password", placeholder: "Password" }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                if with_password {
                    button { class: "btn-primary", style: "height:42px", r#type: "submit", disabled: busy(), "Confirm" }
                }
                if webauthn::supported() {
                    if with_password {
                        div { class: "or-divider", "or" }
                    }
                    button { class: "btn-secondary", style: "height:42px", r#type: "button", disabled: busy(), onclick: with_passkey,
                        icons::Key {}
                        "Use a passkey"
                    }
                } else if !with_password && providers.is_empty() {
                    p { class: "m-0 text-xs text-ctp-yellow",
                        "This browser cannot use passkeys here. Sign out and sign in again with your passkey where it can."
                    }
                }
                for provider in providers {
                    button {
                        class: "btn-outline",
                        style: "height:42px",
                        r#type: "button",
                        disabled: busy(),
                        onclick: {
                            let provider = provider.clone();
                            move |_| with_provider.call(provider.clone())
                        },
                        icons::ShieldCheck { size: 16 }
                        "Continue with {provider}"
                    }
                }
                button { class: "btn-plain", r#type: "button", onclick: move |_| on_cancel.call(()), "Cancel" }
            }
        }
    }
}

/// "Confirm it's you" for a change an admin makes on another page (PX-10,
/// PX-09 finding 3), opened while `asking` holds what the change is ("resetting
/// a password"). Once confirmed it closes and `on_confirmed` tries the
/// change again; cancelled, it closes and nothing is tried. The password
/// is offered if the signed-in user has one now.
#[component]
pub(super) fn ConfirmItsYou(
    asking: Signal<Option<String>>,
    on_confirmed: EventHandler<()>,
) -> Element {
    let mut asking = asking;
    let me = use_resource(|| async {
        let client = rpc::client().await?;
        client.who_am_i().await.map_err(rpc::error)
    });
    let Some(about) = asking() else {
        return rsx! {};
    };
    let with_password = match &*me.read() {
        None => return rsx! {},
        Some(Ok(user)) => user.has_password,
        // Offered anyway: the server says if it is wrong.
        Some(Err(_)) => true,
    };
    rsx! {
        ConfirmDialog {
            with_password,
            about,
            on_confirmed: move |_| {
                asking.set(None);
                on_confirmed.call(());
            },
            on_cancel: move |_| asking.set(None),
        }
    }
}

/// Whether `e` asks for a recent sign-in: if so, `asking` is set to
/// `about`, which opens [`ConfirmItsYou`].
pub(super) fn asks_to_confirm(
    e: &ApiError,
    mut asking: Signal<Option<String>>,
    about: &str,
) -> bool {
    let asks = e.code == error_code::RECENT_SIGN_IN_REQUIRED;
    if asks {
        asking.set(Some(about.to_owned()));
    }
    asks
}

/// The design's "Web · Add a passkey": name it, see the host it binds
/// to, and make it. The server has begun the registration already
/// (`options`), so the browser's prompt opens straight from the button.
#[component]
fn AddPasskeyDialog(
    options: Value,
    host: Option<PasskeyHost>,
    on_added: EventHandler<String>,
    on_cancel: EventHandler<()>,
) -> Element {
    let name = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let on_localhost = host.as_ref().is_some_and(|h| h.rp_id == "localhost");
    let submit = move |e: FormEvent| {
        e.prevent_default();
        let chosen = name().trim().to_owned();
        if chosen.is_empty() {
            error.set(Some(
                "Give it a name first, such as the device it is on.".into(),
            ));
            return;
        }
        busy.set(true);
        // Opened here, in the click, before anything is awaited: Safari
        // opens a passkey prompt only from the click itself.
        let opened = webauthn::open_create(&options);
        spawn(async move {
            let made = match opened {
                Ok(prompt) => prompt.answer().await,
                Err(text) => Err(text),
            };
            match made {
                // Cancelled, or the browser refused: the request still
                // stands, so the button can be pressed again.
                Err(text) => error.set(Some(text)),
                Ok(made) => {
                    let kept = async {
                        let client = rpc::client().await?;
                        client
                            .passkey_add_finish(chosen, made)
                            .await
                            .map_err(rpc::error)
                    };
                    match kept.await {
                        Ok(passkey) => on_added.call(passkey.name),
                        Err(e) => error.set(Some(format!("{} Start again.", e.explain()))),
                    }
                }
            }
            busy.set(false);
        });
    };
    rsx! {
        div { class: "scrim",
            form { class: "dialog dialog-wide", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Key { size: 22 }
                    h1 { "Add a passkey" }
                }
                Field { label: "Name", value: name, autocomplete: "off", placeholder: "MacBook Touch ID" }
                if let Some(host) = &host {
                    div { class: "row",
                        span { class: "row-what",
                            b { "Bound to" }
                            span { class: "row-sub", "A passkey works only at the address it was made on." }
                        }
                        span { class: "row-do",
                            span { class: "mono text-[13px] text-ctp-yellow", "{host.rp_id}" }
                        }
                    }
                }
                if on_localhost {
                    Notice {
                        "This passkey would be bound to "
                        b { "localhost" }
                        ", so it only works when Spoolframe is opened at localhost. Once the server has a real address, open the web UI there and add one again."
                    }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "flex justify-end gap-2.5",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_cancel.call(()), "Cancel" }
                    if on_localhost {
                        button { class: "btn-dialog btn-dialog-outline", r#type: "submit", disabled: busy(), "Continue anyway" }
                    } else {
                        button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(), "Continue" }
                    }
                }
            }
        }
    }
}

/// A change to the PIN, kept while the user confirms it is them: each
/// needs a recent sign-in (P3-03).
#[derive(Debug, Clone, PartialEq)]
enum PinChange {
    Set(String),
    Remove,
    /// No longer asked for on this device (its endpoint id).
    NotRequired(String),
}

async fn devices() -> Result<Vec<DeviceEntry>, ApiError> {
    let client = rpc::client().await?;
    client.devices().await.map_err(rpc::error)
}

/// The design's "Profile PIN" card (P2-08): set, change or remove it,
/// and for each of the user's devices whether it is asked for there.
#[component]
fn PinCard(has_password: Signal<bool>, has_pin: bool) -> Element {
    let mut has_pin = use_signal(|| has_pin);
    let mut list = use_resource(devices);
    let mut editing = use_signal(|| false);
    let mut removing = use_signal(|| false);
    let mut notice = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut confirming = use_signal(|| None::<PinChange>);
    let attempt = use_callback(move |change: PinChange| {
        spawn(async move {
            let tried = async {
                let client = rpc::client().await?;
                match &change {
                    PinChange::Set(pin) => {
                        client.pin_set(pin.clone()).await.map_err(rpc::error)?;
                        notice.set(Some(
                            if has_pin() {
                                "PIN changed."
                            } else {
                                "PIN set."
                            }
                            .into(),
                        ));
                        has_pin.set(true);
                        editing.set(false);
                    }
                    PinChange::Remove => {
                        client.pin_clear().await.map_err(rpc::error)?;
                        notice.set(Some("PIN removed. No device asks for it now.".into()));
                        has_pin.set(false);
                    }
                    PinChange::NotRequired(device) => {
                        client
                            .set_pin_required(device.clone(), false)
                            .await
                            .map_err(rpc::error)?;
                    }
                }
                list.restart();
                Ok::<(), ApiError>(())
            };
            match tried.await {
                Ok(()) => {
                    confirming.set(None);
                    error.set(None);
                }
                Err(e) if e.code == error_code::RECENT_SIGN_IN_REQUIRED => {
                    confirming.set(Some(change));
                }
                Err(e) => {
                    confirming.set(None);
                    error.set(Some(e.explain()));
                }
            }
        });
    });
    let require = move |device: String| {
        spawn(async move {
            let set = async {
                let client = rpc::client().await?;
                client
                    .set_pin_required(device, true)
                    .await
                    .map_err(rpc::error)
            };
            match set.await {
                Ok(()) => {
                    error.set(None);
                    list.restart();
                }
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };
    let current = list.read().clone();
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::Lock { size: 18 }
                h2 { "Profile PIN" }
            }
            div { class: "card-body",
                div { class: "row",
                    span { class: "row-what",
                        b { "Profile PIN" }
                        span { class: "row-sub",
                            if has_pin() { "Set" } else { "Not set: anyone at your devices can open your profile" }
                        }
                    }
                    span { class: "row-do",
                        if removing() {
                            span { class: "text-[13px]", "Remove it?" }
                            button {
                                class: "btn btn-danger",
                                onclick: move |_| {
                                    removing.set(false);
                                    attempt.call(PinChange::Remove);
                                },
                                "Remove"
                            }
                            button { class: "btn", onclick: move |_| removing.set(false), "Keep" }
                        } else if !editing() {
                            button {
                                class: "btn",
                                onclick: move |_| {
                                    notice.set(None);
                                    editing.set(true);
                                },
                                if has_pin() { "Change" } else { "Set a PIN" }
                            }
                            if has_pin() {
                                button { class: "btn btn-danger", onclick: move |_| removing.set(true), "Remove" }
                            }
                        }
                    }
                }
                if editing() {
                    PinForm {
                        on_save: move |pin: String| attempt.call(PinChange::Set(pin)),
                        on_cancel: move |_| editing.set(false),
                    }
                }
                if let Some(text) = notice() {
                    p { class: "m-0 text-xs text-ctp-green", role: "status", "{text}" }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "row-label", "Required on this device" }
                match current {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(devices)) if devices.is_empty() => rsx! {
                        p { class: "m-0 text-sm text-ctp-subtext0", "No devices yet. Sign in with a Spoolframe app, and it is listed here." }
                    },
                    Some(Ok(devices)) => rsx! {
                        for device in devices {
                            div { class: "row", key: "{device.endpoint_id}",
                                span { class: "row-what",
                                    span { class: "break-words", icons::Laptop {} "{device_name(&device.name)}" }
                                    span { class: "row-sub", "Last used {ago(now(), device.last_seen_at)}" }
                                }
                                span { class: "row-do",
                                    button {
                                        class: "switch",
                                        r#type: "button",
                                        role: "switch",
                                        aria_checked: "{device.pin_required}",
                                        aria_label: "Ask for the PIN on {device_name(&device.name)}",
                                        disabled: !has_pin() && !device.pin_required,
                                        onclick: {
                                            let id = device.endpoint_id.clone();
                                            let on = device.pin_required;
                                            move |_| {
                                                if on {
                                                    attempt.call(PinChange::NotRequired(id.clone()));
                                                } else {
                                                    require(id.clone());
                                                }
                                            }
                                        },
                                    }
                                }
                            }
                        }
                        if !has_pin() {
                            p { class: "m-0 text-xs text-ctp-subtext0", "Set a PIN to ask for it on a device." }
                        }
                    },
                }
            }
        }
        if let Some(change) = confirming() {
            ConfirmDialog {
                with_password: has_password(),
                about: "changing your PIN",
                on_confirmed: move |_| attempt.call(change.clone()),
                on_cancel: move |_| confirming.set(None),
            }
        }
    }
}

/// What a user without a password signs in with, as the card warns of
/// it; `None` while that is not known.
fn only_ways(passkeys: bool, links: bool) -> Option<&'static str> {
    match (passkeys, links) {
        (true, false) => Some("Your passkeys are your only way in."),
        (false, true) => Some("Your sign-in provider is your only way in."),
        (true, true) => Some("Your passkeys and sign-in provider are your only ways in."),
        (false, false) => None,
    }
}

/// A link's line under its provider: who it is there, and since when.
fn link_line(link: &SsoLink) -> String {
    let since = format!("linked {}", date(link.linked_at));
    match &link.display {
        Some(who) => format!("{who} · {since}"),
        None => since,
    }
}

/// What a device is called, or what stands for it when it gave no name.
fn device_name(name: &Option<String>) -> String {
    name.clone()
        .unwrap_or_else(|| "A device with no name".into())
}

/// A new PIN, typed twice: 4 to 8 digits.
#[component]
fn PinForm(on_save: EventHandler<String>, on_cancel: EventHandler<()>) -> Element {
    let pin = use_signal(String::new);
    let again = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let submit = move |e: FormEvent| {
        e.prevent_default();
        let typed = pin();
        if !(4..=8).contains(&typed.len()) || !typed.bytes().all(|b| b.is_ascii_digit()) {
            error.set(Some("A PIN is 4 to 8 digits.".into()));
            return;
        }
        if typed != again() {
            error.set(Some("The PINs are not the same.".into()));
            return;
        }
        error.set(None);
        on_save.call(typed);
    };
    rsx! {
        form { class: "flex flex-col gap-3", onsubmit: submit,
            Field { label: "New PIN", value: pin, kind: "password", autocomplete: "off", placeholder: "4 to 8 digits" }
            Field { label: "New PIN again", value: again, kind: "password", autocomplete: "off" }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "flex justify-end gap-2.5",
                button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_cancel.call(()), "Cancel" }
                button { class: "btn-dialog btn-dialog-primary", r#type: "submit", "Save PIN" }
            }
        }
    }
}

async fn sessions() -> Result<Vec<SessionEntry>, ApiError> {
    let client = rpc::client().await?;
    client.sessions().await.map_err(rpc::error)
}

/// The design's "Devices and sessions" card: every live session of the
/// user's, the apps' and the browsers', each signed out by itself, or all
/// but this one at once.
#[component]
fn Sessions() -> Element {
    let mut list = use_resource(sessions);
    let mut error = use_signal(|| None::<String>);
    let mut notice = use_signal(|| None::<String>);
    let mut asking = use_signal(|| false);
    let sign_out = move |session: SessionEntry| {
        spawn(async move {
            let done = async {
                let client = rpc::client().await?;
                client
                    .revoke_session(session.id.clone())
                    .await
                    .map_err(rpc::error)
            };
            match done.await {
                Ok(()) => {
                    error.set(None);
                    notice.set(Some(format!("{} is signed out.", session_name(&session))));
                    list.restart();
                }
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };
    let everywhere_else = move |_| {
        asking.set(false);
        spawn(async move {
            let done = async {
                let client = rpc::client().await?;
                client.revoke_other_sessions().await.map_err(rpc::error)
            };
            match done.await {
                Ok(n) => {
                    error.set(None);
                    notice.set(Some(match n {
                        0 => "Nothing else was signed in.".into(),
                        1 => "Signed out of 1 other session.".into(),
                        n => format!("Signed out of {n} other sessions."),
                    }));
                    list.restart();
                }
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };
    let current = list.read().clone();
    let others = matches!(&current, Some(Ok(s)) if s.iter().any(|s| !s.current));
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::Monitor { size: 18 }
                h2 { "Devices and sessions" }
            }
            div { class: "card-body",
                match current {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(sessions)) => rsx! {
                        for session in sessions {
                            div { class: "row", key: "{session.id}",
                                span { class: "row-what",
                                    b { class: "break-words", "{session_name(&session)}" }
                                    span { class: "row-sub", "{session_line(&session)}" }
                                }
                                span { class: "row-do",
                                    if session.current {
                                        span { class: "text-xs text-ctp-green", "This device" }
                                    } else {
                                        button {
                                            class: "btn btn-danger",
                                            onclick: {
                                                let session = session.clone();
                                                move |_| sign_out(session.clone())
                                            },
                                            "Sign out"
                                        }
                                    }
                                }
                            }
                        }
                    },
                }
                if let Some(text) = notice() {
                    p { class: "m-0 text-xs text-ctp-green", role: "status", "{text}" }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                if asking() {
                    div { class: "flex flex-col gap-2",
                        p { class: "m-0 text-[13px] text-ctp-subtext1",
                            "Every other browser and device is signed out, and has to sign in again. This one stays signed in."
                        }
                        div { class: "flex justify-end gap-2",
                            button { class: "btn", onclick: move |_| asking.set(false), "Keep them" }
                            button { class: "btn btn-danger", onclick: everywhere_else, icons::LogOut {} "Sign out everywhere else" }
                        }
                    }
                } else {
                    button {
                        class: "btn btn-danger btn-wide",
                        disabled: !others,
                        onclick: move |_| asking.set(true),
                        icons::LogOut {}
                        "Sign out everywhere else"
                    }
                }
            }
        }
    }
}

/// A session as the card names it: its device, or a browser or the CLI.
fn session_name(session: &SessionEntry) -> String {
    match (&session.device_name, &session.endpoint_id) {
        (Some(name), _) => name.clone(),
        (None, Some(_)) => "A device with no name".into(),
        (None, None) => "A browser or the CLI".into(),
    }
}

/// "Linux app · 2 hours ago".
fn session_line(session: &SessionEntry) -> String {
    let kind = client_kind(session.endpoint_id.as_deref(), session.platform);
    let when = if session.current {
        "active now".into()
    } else {
        ago(now(), session.last_used_at)
    };
    format!("{kind} · {when}")
}

async fn track_preferences() -> Result<TrackPreferences, ApiError> {
    let client = rpc::client().await?;
    client.track_preferences().await.map_err(rpc::error)
}

async fn track_overrides() -> Result<Vec<TrackOverrideEntry>, ApiError> {
    let client = rpc::client().await?;
    client.track_overrides().await.map_err(rpc::error)
}

/// The design's "Playback" card (P1-20): how tracks are chosen on every
/// device of the user's, saved as each choice is made, and the picks kept
/// for one series or movie, each with "Reset".
#[component]
fn Playback() -> Element {
    let mut loaded = use_resource(track_preferences);
    let mut overrides = use_resource(track_overrides);
    let mut said = use_signal(|| None::<Result<String, String>>);
    let mut busy = use_signal(|| false);
    let save = use_callback(move |preferences: TrackPreferences| {
        busy.set(true);
        spawn(async move {
            let set = async {
                let client = rpc::client().await?;
                client
                    .set_track_preferences(preferences)
                    .await
                    .map_err(rpc::error)
            };
            match set.await {
                Ok(()) => said.set(Some(Ok(
                    "Saved. Your next playback on any device uses it.".into()
                ))),
                Err(e) => said.set(Some(Err(e.explain()))),
            }
            loaded.restart();
            busy.set(false);
        });
    });
    let reset = move |id: String| {
        spawn(async move {
            let done = async {
                let client = rpc::client().await?;
                client.clear_track_override(id).await.map_err(rpc::error)
            };
            if let Err(e) = done.await {
                said.set(Some(Err(e.explain())));
            }
            overrides.restart();
        });
    };
    let current = loaded.read().clone();
    let picks = overrides.read().clone();
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::Captions {}
                h2 { "Playback" }
            }
            div { class: "card-body",
                match current {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(p)) => rsx! {
                        PreferenceRows { preferences: p, busy: busy(), on_change: move |p| save.call(p) }
                    },
                }
                match said() {
                    Some(Ok(text)) => rsx! { p { class: "m-0 text-xs text-ctp-green", role: "status", "{text}" } },
                    Some(Err(text)) => rsx! { Alert { text } },
                    None => rsx! {},
                }
                div { class: "row-label", "For one series or movie" }
                match picks {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        p { class: "m-0 text-sm text-ctp-subtext0",
                            "None yet. Tracks you pick while watching a series are kept for the rest of it, and listed here."
                        }
                    },
                    Some(Ok(list)) => rsx! {
                        for pick in list {
                            div { class: "row", key: "{pick.id}",
                                span { class: "row-what",
                                    b { class: "break-words", "{pick.title.clone().unwrap_or_else(|| pick.id.clone())}" }
                                    span { class: "row-sub", "{override_line(&pick)}" }
                                }
                                span { class: "row-do",
                                    button {
                                        class: "btn",
                                        onclick: {
                                            let id = pick.id.clone();
                                            move |_| reset(id.clone())
                                        },
                                        icons::RotateCcw {}
                                        "Reset"
                                    }
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// The preferences' choices; `on_change` gets them with one changed.
#[component]
fn PreferenceRows(
    preferences: TrackPreferences,
    busy: bool,
    on_change: EventHandler<TrackPreferences>,
) -> Element {
    let audio = preferences::key(&preferences.audio.languages);
    let subtitles = preferences::key(&preferences.subtitles.languages);
    let mode = index_of(SUBTITLE_MODES, &preferences.subtitles.mode);
    let sdh = index_of(SDH, &preferences.subtitles.sdh);
    let avoid = avoids_extras(&preferences.audio.avoid);
    let p = preferences.clone();
    let set_audio = move |e: FormEvent| {
        let mut p = p.clone();
        p.audio.languages = preferences::codes(&e.value());
        on_change.call(p);
    };
    let p = preferences.clone();
    let set_avoid = move |_| {
        let mut p = p.clone();
        if avoid {
            p.audio.avoid.retain(|r| !preferences::AVOIDED.contains(r));
        } else {
            for role in preferences::AVOIDED {
                if !p.audio.avoid.contains(&role) {
                    p.audio.avoid.push(role);
                }
            }
        }
        on_change.call(p);
    };
    let p = preferences.clone();
    let set_mode = move |e: FormEvent| {
        let mut p = p.clone();
        if let Some((mode, _)) = e
            .value()
            .parse()
            .ok()
            .and_then(|i: usize| SUBTITLE_MODES.get(i))
        {
            p.subtitles.mode = *mode;
        }
        on_change.call(p);
    };
    let p = preferences.clone();
    let set_subtitles = move |e: FormEvent| {
        let mut p = p.clone();
        p.subtitles.languages = preferences::codes(&e.value());
        on_change.call(p);
    };
    let p = preferences.clone();
    let set_sdh = move |e: FormEvent| {
        let mut p = p.clone();
        if let Some((sdh, _)) = e.value().parse().ok().and_then(|i: usize| SDH.get(i)) {
            p.subtitles.sdh = *sdh;
        }
        on_change.call(p);
    };
    rsx! {
        div { class: "row",
            span { class: "row-what", b { "Audio language" } }
            span { class: "row-do",
                select { class: "pref-select", aria_label: "Audio language", disabled: busy, onchange: set_audio,
                    for (value, label) in audio_choices(&preferences.audio.languages) {
                        option { value: "{value}", selected: value == audio, "{label}" }
                    }
                }
            }
        }
        div { class: "row",
            span { class: "row-what",
                b { "Avoid audio description and commentary" }
                span { class: "row-sub", "Chosen only when nothing else is in your language" }
            }
            span { class: "row-do",
                button {
                    class: "switch",
                    r#type: "button",
                    role: "switch",
                    aria_checked: "{avoid}",
                    aria_label: "Avoid audio description and commentary",
                    disabled: busy,
                    onclick: set_avoid,
                }
            }
        }
        div { class: "row",
            span { class: "row-what", b { "Subtitles" } }
            span { class: "row-do",
                select { class: "pref-select", aria_label: "Subtitles", disabled: busy, onchange: set_mode,
                    for (i, (_, label)) in SUBTITLE_MODES.iter().enumerate() {
                        option { value: "{i}", selected: i == mode, "{label}" }
                    }
                }
            }
        }
        div { class: "row",
            span { class: "row-what", b { "Subtitle language" } }
            span { class: "row-do",
                select { class: "pref-select", aria_label: "Subtitle language", disabled: busy, onchange: set_subtitles,
                    for (value, label) in subtitle_choices(&preferences.subtitles.languages) {
                        option { value: "{value}", selected: value == subtitles, "{label}" }
                    }
                }
            }
        }
        div { class: "row",
            span { class: "row-what",
                b { "Subtitles for the deaf (SDH)" }
            }
            span { class: "row-do",
                select { class: "pref-select", aria_label: "Subtitles for the deaf", disabled: busy, onchange: set_sdh,
                    for (i, (_, label)) in SDH.iter().enumerate() {
                        option { value: "{i}", selected: i == sdh, "{label}" }
                    }
                }
            }
        }
    }
}
