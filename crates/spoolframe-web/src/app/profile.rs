//! "My profile" (P3-03), as the design's "Web · My profile" board lays it
//! out: the signed-in user's own account, over the `me.*` methods. So far
//! the account and the sign-in methods, the password and passkeys
//! (P3-05); the PIN, devices and playback cards are their own tickets'.

use dioxus::prelude::*;
use serde_json::Value;
use spoolframe_admin_api::{
    MeApiClient, PasskeyEntry, PasskeyHost, PasswordChange, UserInfo, error_code,
};

use super::{Alert, Field, Notice, http, icons, now, rpc, webauthn};
use crate::logic::{ApiError, ago, date, page_serves};

#[component]
pub fn Profile() -> Element {
    let user = use_context::<UserInfo>();
    // Read at sign-in, and kept here: adding a first password changes it.
    let has_password = use_signal(|| user.has_password);
    rsx! {
        div { class: "flex flex-col gap-[18px]",
            h1 { class: "page-title hidden md:block", "My profile" }
            div { class: "card-grid",
                Account { user: user.clone(), has_password }
                Passkeys { has_password }
            }
        }
    }
}

/// Who the user is, and the password, or adding one where there is none
/// (set up with a passkey).
#[component]
fn Account(user: UserInfo, has_password: Signal<bool>) -> Element {
    let mut has_password = has_password;
    let mut changing = use_signal(|| false);
    let mut notice = use_signal(|| None::<String>);
    let shown = user.display_name.clone().unwrap_or(user.username.clone());
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::User {}
                h2 { "Account" }
            }
            div { class: "card-body",
                div { class: "row",
                    span { class: "row-what", b { "Display name" } }
                    span { class: "row-do", span { class: "text-[13px] text-ctp-subtext0", "{shown}" } }
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

/// The sign-in methods card: the password, and the passkeys.
#[component]
fn Passkeys(has_password: Signal<bool>) -> Element {
    let mut list = use_resource(passkeys);
    let host = use_resource(host);
    let mut notice = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
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
                    Notice {
                        b { "Your passkeys are your only way in. " }
                        "If they are lost, getting back in needs an admin, or for the only admin, shell access to the server. Add a password as well, under Account."
                    }
                }
                div { class: "row-label", "Passkeys" }
                if let Some(text) = notice() {
                    p { class: "m-0 text-sm text-ctp-green", role: "status", "{text}" }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
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
                p { class: "m-0 text-xs text-ctp-subtext0", "You can't remove your last sign-in method." }
            }
        }
        if let Some(change) = confirming() {
            ConfirmDialog {
                with_password: has_password(),
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
                    b { "{passkey.name}" }
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
/// passkey `with_password` false, for a user who has none.
#[component]
fn ConfirmDialog(
    with_password: bool,
    on_confirmed: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
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
        spawn(async move {
            let confirmed = async {
                let challenge = http::passkey_begin().await.map_err(|e| e.explain())?;
                let signed = webauthn::get(&challenge.options).await?;
                let client = rpc::client().await.map_err(|e| e.explain())?;
                client
                    .confirm_passkey(challenge.ceremony, signed)
                    .await
                    .map_err(|e| rpc::error(e).explain())
            };
            match confirmed.await {
                Ok(()) => on_confirmed.call(()),
                Err(text) => error.set(Some(text)),
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
                    "You signed in more than 10 minutes ago. Confirm before changing your sign-in methods."
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
                } else if !with_password {
                    p { class: "m-0 text-xs text-ctp-yellow",
                        "This browser cannot use passkeys here. Sign out and sign in again with your passkey where it can."
                    }
                }
                button { class: "btn-plain", r#type: "button", onclick: move |_| on_cancel.call(()), "Cancel" }
            }
        }
    }
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
        let options = options.clone();
        spawn(async move {
            match webauthn::create(&options).await {
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
