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
    SsoLink, TrackOverrideEntry, TrackPreferences, TrackPreferencesInfo, UserInfo, error_code,
};

use super::admin::{Said, Toast, say};
use super::{Alert, Field, Notice, ShownName, http, icons, now, rpc, webauthn};
use crate::logic::{
    ApiError, ago, avatar_class, can_remove_password, client_kind, date, page_serves,
    passkey_title, password_remove_error,
};
use crate::preferences::{
    self, PICTURE_SUBTITLES, SDH, SUBTITLE_MODES, audio_choices, avoids_extras, index_of,
    override_line, picture_row, subtitle_choices,
};
use crate::sso::Tone;
use crate::trust::{self, Change, Icon, LegendLine, Question, Row, Switch, Whose};

#[component]
pub fn Profile() -> Element {
    let user = use_context::<UserInfo>();
    let ShownName(shown) = use_context::<ShownName>();
    // Read at sign-in, and kept here: adding a first password changes it.
    let has_password = use_signal(|| user.has_password);
    // Kept here: the PIN card sets and removes it, and the devices card
    // asks for it only once there is one.
    let has_pin = use_signal(|| user.has_pin);
    // What a sign-in provider's callback sent the browser here to say:
    // put in words by the sign-in methods card, once it knows the
    // server's providers (A-16).
    let said = use_hook(|| http::sso_outcome("/profile"));
    if user.role == Role::Admin {
        // In the admin shell (WebAdminProfile): two columns on a wide
        // screen, one on a phone in `order`.
        return rsx! {
            div { class: "flex flex-col gap-[18px]",
                h1 { class: "page-title hidden md:block", "My profile" }
                div { class: "profile-grid",
                    div { class: "profile-col",
                        div { style: "order:1", Account { user: user.clone(), has_password } }
                        div { style: "order:3", PinCard { has_password, has_pin } }
                        div { style: "order:5", Playback {} }
                    }
                    div { class: "profile-col",
                        div { style: "order:2", Passkeys { has_password, said: said.clone() } }
                        div { style: "order:4", DevicesAndSessions { has_pin } }
                        div { style: "order:6", super::plex::PlexAccountCard {} }
                        div { style: "order:7", MyPortable { tied: user.portable.clone() } }
                        div { style: "order:8", super::connected::ConnectedApps { whose: super::connected::Whose::Mine } }
                    }
                }
            }
        };
    }
    // A consumer's one page (WebProfile, WebProfilePhone): one column.
    let avatar = avatar_class(&user);
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
            MyPortable { tied: user.portable.clone() }
            super::plex::PlexAccountCard {}
            DevicesAndSessions { has_pin }
            PinCard { has_password, has_pin }
            Playback {}
            super::connected::ConnectedApps { whose: super::connected::Whose::Mine }
        }
    }
}

/// The signed-in user's ties to portable identities (P7-04, P7-09), as
/// read at sign-in and kept here: severing one takes its row away.
#[component]
fn MyPortable(tied: Vec<spoolframe_admin_api::PortableTie>) -> Element {
    let toast = use_signal(|| None::<Said>);
    let mut ties = use_signal(|| tied);
    rsx! {
        super::portable::PortableCard {
            owner: crate::portable::Owner::Me,
            username: String::new(),
            ties: ties(),
            toast,
            on_severed: move |id: String| ties.write().retain(|t| t.id != id),
        }
        Toast { said: toast }
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
    /// Remove the password: a passkey or a linked provider signs the user
    /// in from then on.
    RemovePassword,
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
fn Passkeys(has_password: Signal<bool>, said: Option<(String, Option<String>)>) -> Element {
    let mut has_password = has_password;
    let mut list = use_resource(passkeys);
    let host = use_resource(host);
    let mut links = use_resource(sso_links);
    let site = use_resource(http::site);
    let mut notice = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    // What the provider's callback said, shown once the server's providers
    // are known: `?provider=` is anyone's to write, and only a configured
    // provider is named (A-16).
    let mut unsaid = use_signal(|| said);
    use_effect(move || {
        let Some(loaded) = &*site.read() else {
            return;
        };
        let Some((what, provider)) = unsaid.write().take() else {
            return;
        };
        let configured = loaded
            .as_ref()
            .map(|s| s.sso.as_slice())
            .unwrap_or_default();
        match crate::sso::profile_message(&what, provider.as_deref(), configured) {
            Some((Tone::Good, text)) => notice.set(Some(text)),
            Some((Tone::Bad, text)) => error.set(Some(text)),
            None => {}
        }
    });
    // The registration the server began, while the dialog naming the new
    // passkey is open.
    let mut adding = use_signal(|| None::<Value>);
    let mut confirming = use_signal(|| None::<Sensitive>);
    // "Remove it?" asked of the password, until answered.
    let mut removing_password = use_signal(|| false);
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
                    Sensitive::RemovePassword => {
                        client.password_remove().await.map_err(rpc::error)?;
                        has_password.set(false);
                        notice.set(Some(
                            "Password removed. Your other sessions have ended.".into(),
                        ));
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
                    error.set(Some(match change {
                        Sensitive::RemovePassword => password_remove_error(&e, None),
                        _ => e.explain(),
                    }));
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
                        if removing_password() {
                            span { class: "text-[13px]", "Remove it?" }
                            button {
                                class: "btn btn-danger",
                                onclick: move |_| {
                                    removing_password.set(false);
                                    notice.set(None);
                                    attempt.call(Sensitive::RemovePassword);
                                },
                                "Remove"
                            }
                            button { class: "btn", onclick: move |_| removing_password.set(false), "Keep" }
                        } else if has_password() {
                            span { class: "text-xs text-ctp-green", "Set" }
                            // Only beside another way in: the server never
                            // takes the last one.
                            if can_remove_password(has_password(), has_passkeys, has_links) {
                                button { class: "btn btn-danger", onclick: move |_| removing_password.set(true), icons::Trash {} "Remove password" }
                            }
                        } else {
                            span { class: "text-xs text-ctp-subtext0", "Not set" }
                        }
                    }
                }
                if removing_password() {
                    span { class: "hint",
                        "You'll sign in with your passkey or sign-in provider from then on. Your other sessions end, and your trusted devices are forgotten."
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
                if let Some(note) = crate::logic::unusable_note(&passkey, "this server's") {
                    span { class: "row-sub text-ctp-yellow", "{note}" }
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
}

async fn devices() -> Result<Vec<DeviceEntry>, ApiError> {
    let client = rpc::client().await?;
    client.devices().await.map_err(rpc::error)
}

/// The design's "Profile PIN" card (P2-08): set, change or remove it.
/// Where it is asked for is each device's, in "Devices and sessions".
#[component]
fn PinCard(has_password: Signal<bool>, has_pin: Signal<bool>) -> Element {
    let mut has_pin = has_pin;
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
                p { class: "m-0 text-xs text-ctp-subtext0",
                    if has_pin() {
                        "Choose where it is asked for in Devices and sessions."
                    } else {
                        "Set a PIN to ask for it on a device."
                    }
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

/// The card's rows: the user's devices and their live sessions, and the
/// sessions of browsers and the CLI.
async fn device_rows() -> Result<Vec<Row>, ApiError> {
    Ok(trust::rows(devices().await?, sessions().await?))
}

/// A change to a device, held while the page asks about it.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    change: Change,
    endpoint_id: String,
    name: String,
    question: Option<Question>,
}

/// The design's "Devices and sessions" card (WebProfile, WebProfilePhone;
/// P4-03): each device the user has signed in on, with whether it asks
/// for the PIN, is trusted to open the profile without a password, and
/// opens it at start, and each live session, signed out by itself or all
/// but this one at once. A browser's row has no PIN or trust: "—".
#[component]
fn DevicesAndSessions(has_pin: Signal<bool>) -> Element {
    let mut list = use_resource(move || {
        // Read again when the PIN comes or goes: removing it asks for it
        // nowhere.
        let _ = has_pin();
        device_rows()
    });
    let toast = use_signal(|| None::<Said>);
    // The change whose question is open.
    let mut pending = use_signal(|| None::<Pending>);
    // "Confirm it's you", and the change it then makes again.
    let confirming = use_signal(|| None::<String>);
    let mut again = use_signal(|| None::<Pending>);
    let make = use_callback(move |change: Pending| {
        spawn(async move {
            let made = async {
                let client = rpc::client().await?;
                match trust::trust_request(change.change) {
                    Some((trusted, auto_login)) => {
                        client
                            .set_device_trust(change.endpoint_id.clone(), trusted, auto_login)
                            .await
                    }
                    None => {
                        client
                            .set_pin_required(
                                change.endpoint_id.clone(),
                                change.change == Change::AskForPin,
                            )
                            .await
                    }
                }
                .map_err(rpc::error)
            };
            match made.await {
                Ok(()) => say(
                    toast,
                    true,
                    trust::done(change.change, &change.name, &Whose::Mine),
                ),
                Err(e) if asks_to_confirm(&e, confirming, &trust::about(&Whose::Mine)) => {
                    again.set(Some(change));
                }
                Err(e) => say(toast, false, e.explain()),
            }
            list.restart();
        });
    });
    let press = use_callback(
        move |(switch, device, name): (Switch, DeviceEntry, String)| {
            let change = trust::change(switch, &device);
            let asked = Pending {
                change,
                question: trust::question(change, &device, &name, &Whose::Mine),
                endpoint_id: device.endpoint_id,
                name,
            };
            if asked.question.is_some() {
                pending.set(Some(asked));
            } else {
                make.call(asked);
            }
        },
    );
    let sign_out = move |(sessions, name, trusted): (Vec<SessionEntry>, String, bool)| {
        spawn(async move {
            let done = async {
                let client = rpc::client().await?;
                for session in sessions {
                    client
                        .revoke_session(session.id)
                        .await
                        .map_err(rpc::error)?;
                }
                Ok::<(), ApiError>(())
            };
            match done.await {
                Ok(()) => say(toast, true, trust::signed_out(&name, trusted)),
                Err(e) => say(toast, false, e.explain()),
            }
            list.restart();
        });
    };
    // The device whose "Delete …?" is open, by endpoint id and name; the
    // one deleted again once "Confirm it's you" is done.
    let mut deleting = use_signal(|| None::<(String, String)>);
    let confirming_delete = use_signal(|| None::<String>);
    let mut delete_again = use_signal(|| None::<(String, String)>);
    let delete = use_callback(move |(endpoint_id, name): (String, String)| {
        spawn(async move {
            let deleted = async {
                let client = rpc::client().await?;
                client
                    .forget_device(endpoint_id.clone())
                    .await
                    .map_err(rpc::error)
            };
            match deleted.await {
                Ok(()) => say(toast, true, trust::forgotten(&name)),
                Err(e)
                    if asks_to_confirm(
                        &e,
                        confirming_delete,
                        &trust::forget_about(&Whose::Mine),
                    ) =>
                {
                    delete_again.set(Some((endpoint_id, name)));
                }
                Err(e) => say(toast, false, e.explain()),
            }
            list.restart();
        });
    });
    let current = list.read().clone();
    let legend = trust::legend(&Whose::Mine, has_pin());
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
                    Some(Ok(rows)) if rows.is_empty() => rsx! {
                        p { class: "m-0 text-sm text-ctp-subtext0", "No devices yet. Sign in with a Spoolframe app, and it is listed here." }
                    },
                    Some(Ok(rows)) => rsx! {
                        div { class: "device-list",
                          div { class: "device-rows",
                            div { class: "device-head", "aria-hidden": "true",
                                span { "Device" }
                                span { class: "device-switches",
                                    for line in legend.iter().cloned() {
                                        span {
                                            key: "{line.term}",
                                            title: "{line.text}",
                                            aria_describedby: trust::legend_id(line.switch),
                                            "{line.term}"
                                        }
                                    }
                                }
                                span {}
                            }
                            for row in rows {
                                DeviceRow {
                                    key: "{row_key(&row)}",
                                    row,
                                    has_pin: has_pin(),
                                    on_press: press,
                                    on_sign_out: sign_out,
                                    on_delete: move |asked| deleting.set(Some(asked)),
                                }
                            }
                        }
                        }
                    },
                }
                // Where WebProfile has its footnote: what each column does,
                // why a browser has none, and when a change asks to
                // confirm it's you.
                DevicesLegend { whose: Whose::Mine, has_pin: has_pin(), browsers: true }
                p { class: "device-footnote",
                    "Changes here ask you to confirm it's you if you signed in over 10 minutes ago."
                }
            }
        }
        if let Some(Pending { question: Some(question), .. }) = pending() {
            TrustDialog {
                question,
                on_yes: move |_| {
                    if let Some(change) = pending.write().take() {
                        make.call(change);
                    }
                },
                on_no: move |_| pending.set(None),
            }
        }
        ConfirmItsYou {
            asking: confirming,
            on_confirmed: move |_| {
                let change = again.write().take();
                if let Some(change) = change {
                    make.call(change);
                }
            },
        }
        if let Some((endpoint_id, name)) = deleting() {
            TrustDialog {
                question: trust::forget_question(&name, &Whose::Mine),
                on_yes: move |_| {
                    deleting.set(None);
                    delete.call((endpoint_id.clone(), name.clone()));
                },
                on_no: move |_| deleting.set(None),
            }
        }
        ConfirmItsYou {
            asking: confirming_delete,
            on_confirmed: move |_| {
                let asked = delete_again.write().take();
                if let Some(asked) = asked {
                    delete.call(asked);
                }
            },
        }
        Toast { said: toast }
    }
}

/// What keeps a row the same row across reloads.
fn row_key(row: &Row) -> String {
    match &row.device {
        Some(device) => device.endpoint_id.clone(),
        None => row
            .sessions
            .first()
            .map(|s| s.id.clone())
            .unwrap_or_default(),
    }
}

/// This browser's `User-Agent`, which names its own session's row.
pub(super) fn user_agent() -> Option<String> {
    web_sys::window().and_then(|w| w.navigator().user_agent().ok())
}

/// One row of "Devices and sessions": a device with its three switches,
/// or a browser's session, which has none. Wide, the switches sit under
/// the card's columns and a browser shows "—" in each; narrow (a phone),
/// each row is a card with "This device" or Sign out at its top right,
/// the switches labelled under them, and a browser says why it has none.
#[component]
fn DeviceRow(
    row: Row,
    has_pin: bool,
    on_press: Callback<(Switch, DeviceEntry, String)>,
    on_sign_out: EventHandler<(Vec<SessionEntry>, String, bool)>,
    on_delete: EventHandler<(String, String)>,
) -> Element {
    let (name, line) = match &row.device {
        Some(device) => {
            let name = device
                .name
                .clone()
                .or_else(|| row.sessions.iter().find_map(|s| s.device_name.clone()))
                .unwrap_or_else(|| "A device with no name".into());
            let kind = client_kind(Some(&device.endpoint_id), device.platform);
            let when = if row.current() {
                format!("{kind} · active now")
            } else {
                match row.sessions.iter().map(|s| s.last_used_at).max() {
                    Some(used) => format!("{kind} · {}", ago(now(), used)),
                    None => format!(
                        "{kind} · not signed in · last seen {}",
                        ago(now(), device.last_seen_at)
                    ),
                }
            };
            (name, when)
        }
        None => match row.sessions.first() {
            Some(session) => (session_name(session), session_line(session)),
            None => (String::new(), String::new()),
        },
    };
    let current = row.current();
    let trusted = row.device.as_ref().is_some_and(|d| d.trusted);
    let legend = trust::legend(&Whose::Mine, has_pin);
    rsx! {
        div { class: "device-row",
            span { class: "row-what device-what",
                b { class: "break-words", "{name}" }
                span { class: "row-sub", "{line}" }
            }
            match &row.device {
                Some(device) => rsx! {
                    span { class: "device-switches",
                        for (LegendLine { switch, term: what, text }, on) in legend.into_iter().zip([
                            device.pin_required,
                            device.trusted,
                            device.auto_login,
                        ]) {
                            span { class: "device-switch", key: "{what}",
                                button {
                                    class: "switch",
                                    r#type: "button",
                                    role: "switch",
                                    aria_checked: "{on}",
                                    aria_label: "{what} on {name}",
                                    aria_describedby: trust::legend_id(switch),
                                    disabled: !trust::can_press(switch, device, has_pin),
                                    title: switch_title(switch, device, has_pin),
                                    onclick: {
                                        let device = device.clone();
                                        let name = name.clone();
                                        move |_| on_press.call((switch, device.clone(), name.clone()))
                                    },
                                }
                                span {
                                    class: "device-switch-label",
                                    aria_hidden: "true",
                                    title: "{text}",
                                    aria_describedby: trust::legend_id(switch),
                                    "{what}"
                                }
                            }
                        }
                    }
                },
                None => rsx! {
                    span { class: "device-switches device-none", aria_hidden: "true",
                        for what in ["PIN", "Trusted", "Auto-login"] {
                            span { class: "device-switch", key: "{what}", "—" }
                        }
                    }
                    span { class: "device-note", "{trust::BROWSER_NOTE}" }
                },
            }
            span { class: "device-do",
                if current {
                    span { class: "text-xs text-ctp-green", "This device" }
                } else if !row.sessions.is_empty() {
                    button {
                        class: "btn btn-danger",
                        title: if trusted { "Signing it out also stops trusting it." } else { "" },
                        onclick: {
                            let sessions = row.sessions.clone();
                            let name = name.clone();
                            move |_| on_sign_out.call((sessions.clone(), name.clone(), trusted))
                        },
                        "Sign out"
                    }
                }
                if let Some(device) = row.device.as_ref().filter(|_| row.can_forget()) {
                    button {
                        class: "btn-delete",
                        aria_label: "Delete {name}",
                        onclick: {
                            let asked = (device.endpoint_id.clone(), name.clone());
                            move |_| on_delete.call(asked.clone())
                        },
                        icons::Trash { size: 14 }
                        "Delete"
                    }
                }
            }
        }
    }
}

/// The legend under a page's device switches (P4-03), where WebProfile
/// has its footnote: each column's name in bold and what it does. Each
/// column head, phone label and switch names its line as their
/// description. With `browsers`, a last line says why a browser's row has
/// no switches, which a wide row shows only as dashes.
#[component]
pub(super) fn DevicesLegend(whose: Whose, has_pin: bool, browsers: bool) -> Element {
    rsx! {
        dl { class: "device-legend",
            for line in trust::legend(&whose, has_pin) {
                div { key: "{line.term}",
                    dt { "{line.term}" }
                    dd { id: trust::legend_id(line.switch), "{line.text}" }
                }
            }
            if browsers {
                div {
                    dt { "—" }
                    dd { "{trust::BROWSER_NOTE}" }
                }
            }
        }
    }
}

/// Why a switch cannot be pressed, as its tooltip.
fn switch_title(switch: Switch, device: &DeviceEntry, has_pin: bool) -> &'static str {
    if trust::can_press(switch, device, has_pin) {
        return "";
    }
    match switch {
        Switch::Pin => "Set a PIN first.",
        Switch::AutoLogin => "Trust the device first.",
        Switch::Trusted => "",
    }
}

/// The question before a change to a device's trust, auto-login or PIN
/// (WebTrustStates).
#[component]
pub(super) fn TrustDialog(
    question: Question,
    on_yes: EventHandler<()>,
    on_no: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                // Taking trust away is a yellow shield on the board, not red:
                // nothing is lost that a sign-in cannot bring back.
                // Deleting is red: the device is forgotten, not just signed out.
                div {
                    class: match (question.icon, question.danger) {
                        (Icon::Delete, _) => "dialog-head dialog-head-danger",
                        (_, true) => "dialog-head dialog-head-warn",
                        (_, false) => "dialog-head",
                    },
                    match question.icon {
                        Icon::Start => rsx! { icons::Play { size: 22 } },
                        Icon::Pin => rsx! { icons::Lock { size: 22 } },
                        Icon::Untrust => rsx! { icons::ShieldAlert { size: 22 } },
                        Icon::Delete => rsx! { icons::Trash { size: 22 } },
                    }
                    h1 { "{question.title}" }
                }
                p { "{question.text}" }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_no.call(()), "Cancel" }
                    button {
                        class: if question.danger { "btn-dialog btn-dialog-outline text-ctp-red" } else { "btn-dialog btn-dialog-primary" },
                        onclick: move |_| on_yes.call(()),
                        "{question.action}"
                    }
                }
            }
        }
    }
}

/// A session as every page names it (logic::session_name): its device,
/// this browser by its own name, or a browser or the CLI.
fn session_name(session: &SessionEntry) -> String {
    crate::logic::session_name(
        session.endpoint_id.as_deref(),
        session.device_name.as_deref(),
        session.platform,
        session.current,
        user_agent().as_deref(),
    )
}

/// "Web browser · 3 days ago".
fn session_line(session: &SessionEntry) -> String {
    let kind = crate::logic::session_kind(session.endpoint_id.as_deref(), session.platform);
    let when = if session.current {
        "active now".into()
    } else {
        ago(now(), session.last_used_at)
    };
    format!("{kind} · {when}")
}

async fn track_preferences() -> Result<TrackPreferencesInfo, ApiError> {
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
                    Some(Ok(info)) => rsx! {
                        PreferenceRows { preferences: info.preferences, ocr_offered: info.ocr_offered, busy: busy(), on_change: move |p| save.call(p) }
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
    ocr_offered: bool,
    busy: bool,
    on_change: EventHandler<TrackPreferences>,
) -> Element {
    let audio = preferences::key(&preferences.audio.languages);
    let subtitles = preferences::key(&preferences.subtitles.languages);
    let mode = index_of(SUBTITLE_MODES, &preferences.subtitles.mode);
    let sdh = index_of(SDH, &preferences.subtitles.sdh);
    let pictures = picture_row(preferences.subtitles.bitmap, ocr_offered);
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
    let p = preferences.clone();
    let set_pictures = move |e: FormEvent| {
        let mut p = p.clone();
        if let Some((bitmap, _)) = e
            .value()
            .parse()
            .ok()
            .and_then(|i: usize| PICTURE_SUBTITLES.get(i))
        {
            p.subtitles.bitmap = *bitmap;
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
        div { class: "row",
            span { class: "row-what",
                b { "Picture subtitles" }
                span { class: "row-sub",
                    {pictures.off.unwrap_or("PGS, VobSub and DVB: as text, any device shows them styled and sized")}
                }
            }
            span { class: "row-do",
                select { class: "pref-select", aria_label: "Picture subtitles", disabled: busy || pictures.off.is_some(), onchange: set_pictures,
                    for (i, (_, label)) in PICTURE_SUBTITLES.iter().enumerate() {
                        option { value: "{i}", selected: i == pictures.shown, "{label}" }
                    }
                }
            }
        }
    }
}
