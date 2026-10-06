//! An admin's pages (P3-02): users, a user's sessions, and the server, as
//! the design's WebUsers, WebUserSessions and WebServer boards lay them
//! out. Everything goes over the admin JSON-RPC API, which the CLI uses
//! too.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, DeviceEntry, NewUser, PathCheckRun, PathReport, Role, SsoLink, UserInfo,
};

use super::profile::{ConfirmItsYou, TrustDialog, asks_to_confirm};
use super::{Alert, Field, Route, icons, initial, now, role_name, rpc};
use crate::logic::{
    AddUserError, ApiError, Landing, PathState, UserStatus, add_user_error, ago, avatar_class,
    client_kind, count, last_admin, path_state, plex_line, server_version, sign_in_methods,
    thousands, user_change_error, user_status,
};
use crate::trust::{self, Change, Question, Switch, Whose};

#[component]
pub fn AdminHome() -> Element {
    navigator().replace(Route::from(Landing::Admin));
    rsx! {}
}

async fn users() -> Result<Vec<UserInfo>, ApiError> {
    let client = rpc::client().await?;
    client.user_list().await.map_err(rpc::error)
}

/// What a change did, or why it was not made: the toast in the corner.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Said {
    ok: bool,
    text: String,
    /// Which toast it is, so an older one's timer leaves a newer one up.
    n: u64,
}

/// How long a toast stays up.
const TOAST_MS: i32 = 6000;

/// Show `text` in the corner, for [`TOAST_MS`] or until dismissed.
pub(super) fn say(mut toast: Signal<Option<Said>>, ok: bool, text: String) {
    let n = toast.peek().as_ref().map_or(0, |t| t.n) + 1;
    toast.set(Some(Said { ok, text, n }));
    spawn(async move {
        sleep(TOAST_MS).await;
        if toast.peek().as_ref().is_some_and(|t| t.n == n) {
            toast.set(None);
        }
    });
}

/// Wait `ms` milliseconds, by the browser's timer.
pub(super) async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

#[component]
pub(super) fn Toast(said: Signal<Option<Said>>) -> Element {
    let mut said = said;
    let Some(shown) = said() else {
        return rsx! {};
    };
    rsx! {
        div { class: if shown.ok { "toast toast-ok" } else { "toast toast-bad" }, role: "status",
            if shown.ok {
                icons::CircleCheck { size: 18 }
            } else {
                icons::CircleX {}
            }
            span { "{shown.text}" }
            button { aria_label: "Dismiss", onclick: move |_| said.set(None), icons::X {} }
        }
    }
}

/// A list that did not load: what, why, and trying again.
#[component]
fn LoadError(what: String, error: ApiError, on_retry: EventHandler<()>) -> Element {
    rsx! {
        section { class: "card",
            div { class: "card-body",
                div { class: "alert alert-error", role: "alert",
                    icons::CircleX {}
                    span { class: "flex flex-col gap-[3px]",
                        b { "{what}" }
                        span { class: "text-ctp-text", "{error.explain()}" }
                    }
                }
                div {
                    button { class: "btn", onclick: move |_| on_retry.call(()),
                        icons::RotateCcw {}
                        "Try again"
                    }
                }
            }
        }
    }
}

/// What the pages call a user: their display name, or their username.
fn shown_name(user: &UserInfo) -> String {
    user.display_name
        .clone()
        .unwrap_or_else(|| user.username.clone())
}

#[component]
pub fn Users() -> Element {
    let mut list = use_resource(users);
    let toast = use_signal(|| None::<Said>);
    let mut adding = use_signal(|| false);
    let mut resetting = use_signal(|| None::<String>);
    let current = list.read().clone();
    rsx! {
        div { class: "flex flex-col gap-6",
            div { class: "page-head hidden md:flex",
                h1 { class: "page-title page-title-admin", "Users and devices" }
                button { class: "btn-add", onclick: move |_| adding.set(true), "Add user" }
            }
            button { class: "btn-primary h-[42px] md:hidden", onclick: move |_| adding.set(true),
                icons::Plus { size: 17 }
                "Add user"
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(error)) => rsx! {
                    LoadError { what: "Couldn't load users", error, on_retry: move |_| list.restart() }
                },
                Some(Ok(users)) => rsx! {
                    UserTable {
                        users: users.clone(),
                        on_reset: move |name: String| resetting.set(Some(name)),
                    }
                    div { class: "flex flex-col gap-3.5 md:hidden",
                        for user in users.iter().cloned() {
                            UserCard { key: "{user.username}", user }
                        }
                    }
                    if users.len() == 1 {
                        OnlyYou { on_add: move |_| adding.set(true) }
                    }
                },
            }
        }
        if adding() {
            AddUser {
                on_done: move |added: Option<String>| {
                    adding.set(false);
                    if let Some(name) = added {
                        say(toast, true, format!("{name} added"));
                        list.restart();
                    }
                },
            }
        }
        if let Some(username) = resetting() {
            ResetPassword {
                username,
                on_done: move |said: Option<(bool, String)>| {
                    resetting.set(None);
                    if let Some((ok, text)) = said {
                        say(toast, ok, text);
                    }
                },
            }
        }
        Toast { said: toast }
    }
}

/// The users as the WebUsers board's table: who, role, how they sign in,
/// their devices and live sessions, and whether they can. Their name opens
/// their page, where everything but a password reset is done.
#[component]
fn UserTable(users: Vec<UserInfo>, on_reset: EventHandler<String>) -> Element {
    rsx! {
        section { class: "table-card hidden md:block",
            table { class: "table",
                thead {
                    tr {
                        th { "User" }
                        th { "Role" }
                        th { "Sign-in" }
                        th { "Devices" }
                        th { "Sessions" }
                        th { "Status" }
                        th {}
                    }
                }
                tbody {
                    for user in users {
                        tr { key: "{user.username}", class: if user.disabled { "off" } else { "" },
                            td {
                                Link { class: "who", to: Route::UserSessions { username: user.username.clone() },
                                    span { class: "{avatar_class(&user.username, user.disabled)} avatar-md", "{initial(&shown_name(&user))}" }
                                    span { class: "who-name",
                                        b { "{shown_name(&user)}" }
                                        if user.display_name.is_some() {
                                            span { "{user.username}" }
                                        }
                                    }
                                }
                            }
                            td { RolePill { role: user.role } }
                            td { "{sign_in_methods(&user)}" }
                            td { "{user.device_count}" }
                            td { "{user.session_count}" }
                            td { Status { user: user.clone() } }
                            td { class: "end",
                                button {
                                    class: "btn h-8 px-3 font-normal",
                                    onclick: {
                                        let name = user.username.clone();
                                        move |_| on_reset.call(name.clone())
                                    },
                                    "Reset password"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A user on a phone (the WebAdminPhone board): a card that opens their
/// page.
#[component]
fn UserCard(user: UserInfo) -> Element {
    let seen = if user.session_count > 0 {
        count(user.session_count, "session")
    } else if user.last_seen_at.is_none() {
        "never signed in".to_owned()
    } else {
        "signed out".to_owned()
    };
    rsx! {
        Link {
            class: if user.disabled { "user-card off" } else { "user-card" },
            to: Route::UserSessions { username: user.username.clone() },
            span { class: "{avatar_class(&user.username, user.disabled)} avatar-card", "{initial(&shown_name(&user))}" }
            span {
                b { "{shown_name(&user)}" }
                span {
                    "{user.username} · "
                    if user.disabled { "disabled" } else { "{seen}" }
                }
            }
            RolePill { role: user.role }
            icons::ChevronRight {}
        }
    }
}

#[component]
fn RolePill(role: Role) -> Element {
    rsx! {
        span { class: if role == Role::Admin { "pill pill-admin" } else { "pill" }, "{role_name(role)}" }
    }
}

#[component]
fn Status(user: UserInfo) -> Element {
    match user_status(&user) {
        UserStatus::Disabled => rsx! { span { class: "pill", "Disabled" } },
        // WebUsers: Steve's row.
        UserStatus::Limited(why) => {
            rsx! { span { class: "state state-warn", title: "{why}", "Rate limited" } }
        }
        UserStatus::Active => rsx! { span { class: "state state-ok", "Active" } },
    }
}

/// The list when it is only the admin (WebUsersStates, 7).
#[component]
fn OnlyYou(on_add: EventHandler<()>) -> Element {
    rsx! {
        section { class: "card empty",
            icons::Users {}
            b { "You're the only user" }
            span { "Add people so each gets their own watch history and Continue watching." }
            button { class: "btn-dialog btn-dialog-primary", onclick: move |_| on_add.call(()),
                icons::Plus { size: 16 }
                "Add a user"
            }
        }
    }
}

/// "Set a new password for <username>" (WebUsersStates, 3). `on_done`
/// gets what to say, or `None` when cancelled.
#[component]
fn ResetPassword(username: String, on_done: EventHandler<Option<(bool, String)>>) -> Element {
    let password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let name = username.clone();
    let attempt = use_callback(move |()| {
        busy.set(true);
        let username = name.clone();
        spawn(async move {
            let set = async {
                let client = rpc::client().await?;
                client
                    .user_set_password(spoolframe_admin_api::Credentials {
                        username: username.clone(),
                        password: password(),
                    })
                    .await
                    .map_err(rpc::error)
            };
            match set.await {
                Ok(()) => on_done.call(Some((true, format!("New password set for {username}")))),
                Err(e) if asks_to_confirm(&e, confirming, "setting a password") => {}
                Err(e) => error.set(Some(user_change_error(&e, &username))),
            }
            busy.set(false);
        });
    });
    let submit = move |e: FormEvent| {
        e.prevent_default();
        attempt.call(());
    };
    rsx! {
        div { class: "scrim",
            form { class: "dialog w-[440px]", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Key { size: 22 }
                    h1 { "Set a new password for {username}" }
                }
                Field { label: "New password", value: password, kind: "password", autocomplete: "new-password" }
                span { class: "hint", "{username} is signed out on every device and browser, and signs in again with this one." }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(), "Set password" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| attempt.call(()) }
    }
}

/// "Add a user" (WebUsersStates, 1 and 2). `on_done` gets who was added,
/// or `None` when cancelled.
#[component]
fn AddUser(on_done: EventHandler<Option<String>>) -> Element {
    let username = use_signal(String::new);
    let display = use_signal(String::new);
    let password = use_signal(String::new);
    let mut admin = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut bad_username = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let attempt = use_callback(move |()| {
        busy.set(true);
        spawn(async move {
            let display = display();
            let typed = username().trim().to_owned();
            let user = NewUser {
                username: username(),
                display_name: (!display.trim().is_empty()).then(|| display.trim().to_owned()),
                password: password(),
                role: if admin() { Role::Admin } else { Role::Consumer },
            };
            let added = async {
                let client = rpc::client().await?;
                client.user_add(user).await.map_err(rpc::error)
            };
            match added.await {
                Ok(user) => on_done.call(Some(shown_name(&user))),
                Err(e) if asks_to_confirm(&e, confirming, "adding an admin") => {}
                Err(e) => match add_user_error(&e, &typed) {
                    AddUserError::Username(text) => {
                        bad_username.set(Some(text));
                        error.set(None);
                    }
                    AddUserError::Form(text) => {
                        bad_username.set(None);
                        error.set(Some(text));
                    }
                },
            }
            busy.set(false);
        });
    });
    let submit = move |e: FormEvent| {
        e.prevent_default();
        attempt.call(());
    };
    let who = match (display().trim(), username().trim()) {
        ("", "") => "They".to_owned(),
        ("", name) | (name, _) => name.to_owned(),
    };
    rsx! {
        div { class: "scrim",
            form { class: "dialog w-[440px]", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Plus { size: 22 }
                    h1 { "Add a user" }
                }
                Field { label: "Username", value: username, autocomplete: "off", error: bad_username() }
                Field { label: "Display name", value: display, autocomplete: "off" }
                label { class: "field",
                    "Role"
                    select { onchange: move |e| admin.set(e.value() == "admin"),
                        option { value: "consumer", selected: !admin(), "Consumer" }
                        option { value: "admin", selected: admin(), "Admin" }
                    }
                }
                Field { label: "Password", value: password, kind: "password", autocomplete: "new-password" }
                span { class: "hint", "{who} can change it, and add a passkey, in My profile." }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(), "Add user" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| attempt.call(()) }
    }
}

/// A change on a user's page that is asked about first.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ask {
    Role(Role),
    Disable,
    ClearPin,
}

/// One user's page (the WebUserSessions board): who, what can be done to
/// the account, their sessions, their devices and the PIN, and their
/// passkeys.
#[component]
pub fn UserSessions(username: String) -> Element {
    let mut list = use_resource(users);
    let current = list.read().clone();
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            Link { class: "back-link", to: Route::Users {}, icons::ChevronLeft {} "Users" }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(error)) => rsx! {
                    LoadError { what: "Couldn't load {username}", error, on_retry: move |_| list.restart() }
                },
                Some(Ok(users)) => match users.iter().find(|u| u.username == username).cloned() {
                    None => rsx! { Alert { text: format!("There is no user {username}.") } },
                    Some(user) => rsx! {
                        UserPage {
                            key: "{user.username}",
                            last: last_admin(&users, &user.username),
                            user,
                            on_changed: move |_| list.restart(),
                        }
                    },
                },
            }
        }
    }
}

#[component]
fn UserPage(user: UserInfo, last: bool, on_changed: EventHandler<()>) -> Element {
    let toast = use_signal(|| None::<Said>);
    let mut asking = use_signal(|| None::<Ask>);
    let mut resetting = use_signal(|| false);
    let username = user.username.clone();
    let name = shown_name(&user);
    let why_not = "The last admin: make someone else an admin first.";
    let (to, role_label) = match user.role {
        Role::Admin => (Role::Consumer, "Make consumer"),
        _ => (Role::Admin, "Make admin"),
    };
    let role_blocked = last && to == Role::Consumer;
    let enable = {
        let username = username.clone();
        move |_| {
            let username = username.clone();
            spawn(async move {
                let enabled = async {
                    let client = rpc::client().await?;
                    client
                        .user_enable(username.clone())
                        .await
                        .map_err(rpc::error)
                };
                match enabled.await {
                    Ok(()) => say(toast, true, format!("{username} enabled")),
                    Err(e) => say(toast, false, user_change_error(&e, &username)),
                }
                on_changed.call(());
            });
        }
    };
    rsx! {
        div { class: "page-head",
            span { class: "who gap-3.5",
                span { class: "{avatar_class(&user.username, user.disabled)} avatar-lg", "{initial(&name)}" }
                span { class: "flex flex-col",
                    h1 { class: "page-title page-title-admin", "{name}" }
                    span { class: "text-sm font-medium text-ctp-subtext0",
                        "{user.username} · {role_name(user.role)} · "
                        match user_status(&user) {
                            UserStatus::Disabled => rsx! { "Disabled" },
                            UserStatus::Limited(why) => rsx! { span { class: "text-ctp-yellow", title: "{why}", "Rate limited" } },
                            UserStatus::Active => rsx! { "Active" },
                        }
                    }
                }
            }
            span { class: "flex flex-wrap gap-2",
                button { class: "btn", onclick: move |_| resetting.set(true), icons::Key { size: 15 } "Reset password" }
                button {
                    class: "btn",
                    disabled: role_blocked,
                    title: if role_blocked { why_not } else { "" },
                    onclick: move |_| asking.set(Some(Ask::Role(to))),
                    icons::UserCog {}
                    "{role_label}"
                }
                if user.disabled {
                    button { class: "btn", onclick: enable, "Enable" }
                } else {
                    button {
                        class: "btn btn-danger",
                        disabled: last,
                        title: if last { why_not } else { "" },
                        onclick: move |_| asking.set(Some(Ask::Disable)),
                        "Disable"
                    }
                }
            }
        }
        UserSessionList { username: username.clone(), name: name.clone(), disabled: user.disabled, toast }
        super::plex::UserPlex { username: username.clone(), name: name.clone(), toast }
        UserSso { username: username.clone(), name: name.clone(), toast }
        UserDevices {
            username: username.clone(),
            has_pin: user.has_pin,
            toast,
            on_clear: move |_| asking.set(Some(Ask::ClearPin)),
        }
        UserPasskeys { username: username.clone(), name: name.clone() }
        if resetting() {
            ResetPassword {
                username: username.clone(),
                on_done: move |said: Option<(bool, String)>| {
                    resetting.set(false);
                    if let Some((ok, text)) = said {
                        say(toast, ok, text);
                    }
                },
            }
        }
        if let Some(ask) = asking() {
            Confirm {
                ask,
                username: username.clone(),
                on_done: move |said: Option<(bool, String)>| {
                    asking.set(None);
                    if let Some((ok, text)) = said {
                        say(toast, ok, text);
                        on_changed.call(());
                    }
                },
            }
        }
        Toast { said: toast }
    }
}

/// The question before a change (WebUsersStates, 4 and 5), and the change.
#[component]
fn Confirm(ask: Ask, username: String, on_done: EventHandler<Option<(bool, String)>>) -> Element {
    let mut busy = use_signal(|| false);
    let (title, text, action) = match ask {
        Ask::Role(Role::Admin) => (
            format!("Make {username} an admin?"),
            format!(
                "Admins manage users, libraries and this server. The admin pages appear at {username}'s next sign-in."
            ),
            "Make admin",
        ),
        Ask::Role(_) => (
            format!("Make {username} a consumer?"),
            format!(
                "{username} can no longer manage users or this server, from now on. Their own account and watch history stay as they are."
            ),
            "Make consumer",
        ),
        Ask::Disable => (
            format!("Disable {username}?"),
            format!(
                "{username} is signed out on every device and can't sign in until enabled again. Watch history and settings are kept."
            ),
            "Disable",
        ),
        Ask::ClearPin => (
            format!("Clear {username}'s PIN?"),
            format!("No device asks for it any more. {username} can set a new one in the app."),
            "Clear PIN",
        ),
    };
    let confirming = use_signal(|| None::<String>);
    let about = match ask {
        Ask::Role(_) => "making an admin",
        _ => "clearing a PIN",
    };
    let name = username.clone();
    let go = use_callback(move |()| {
        busy.set(true);
        let username = name.clone();
        spawn(async move {
            let done: Result<String, ApiError> = async {
                let client = rpc::client().await?;
                match ask {
                    Ask::Role(role) => {
                        let user = client
                            .user_set_role(username.clone(), role)
                            .await
                            .map_err(rpc::error)?;
                        Ok(format!(
                            "{} is now {}",
                            user.username,
                            if user.role == Role::Admin {
                                "an admin"
                            } else {
                                "a consumer"
                            }
                        ))
                    }
                    Ask::Disable => {
                        let ended = client
                            .user_disable(username.clone())
                            .await
                            .map_err(rpc::error)?;
                        Ok(format!(
                            "{username} disabled · signed out of {}",
                            count(ended, "session")
                        ))
                    }
                    Ask::ClearPin => {
                        client
                            .user_clear_pin(username.clone())
                            .await
                            .map_err(rpc::error)?;
                        Ok(format!("{username}'s PIN cleared"))
                    }
                }
            }
            .await;
            busy.set(false);
            if let Err(e) = &done
                && asks_to_confirm(e, confirming, about)
            {
                return;
            }
            on_done.call(Some(match done {
                Ok(text) => (true, text),
                Err(e) => (false, user_change_error(&e, &username)),
            }));
        });
    });
    let danger = ask == Ask::Disable;
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: if danger { "dialog-head dialog-head-danger" } else { "dialog-head" },
                    match ask {
                        Ask::Disable => rsx! { icons::CircleX { size: 22 } },
                        Ask::ClearPin => rsx! { icons::Lock { size: 22 } },
                        Ask::Role(_) => rsx! { icons::UserCog { size: 22 } },
                    }
                    h1 { "{title}" }
                }
                p { "{text}" }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Cancel" }
                    button {
                        class: if danger { "btn-dialog btn-dialog-outline text-ctp-red" } else { "btn-dialog btn-dialog-primary" },
                        disabled: busy(),
                        onclick: move |_| go.call(()),
                        "{action}"
                    }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| go.call(()) }
    }
}

/// What a session's device is called: its name, its key's start, or for
/// an admin API session what signed it in (P3-18), "web / CLI" for one
/// from before the server kept it.
fn session_device(
    endpoint_id: &Option<String>,
    name: &Option<String>,
    platform: Option<spoolframe_admin_api::Platform>,
) -> String {
    use spoolframe_admin_api::Platform;
    match (endpoint_id, name, platform) {
        (Some(_), Some(name), _) => name.clone(),
        (Some(id), None, _) => format!("Device {}", &id[..id.len().min(10)]),
        (None, _, Some(Platform::Web)) => "Browser".to_owned(),
        (None, _, Some(Platform::Cli)) => "Command line".to_owned(),
        (None, _, _) => "web / CLI".to_owned(),
    }
}

#[component]
fn UserSessionList(
    username: String,
    name: String,
    disabled: bool,
    toast: Signal<Option<Said>>,
) -> Element {
    let mut sessions = use_resource(use_reactive(
        (&username, &disabled),
        |(username, disabled)| async move {
            // Read again when the account is disabled: that ends every session.
            let _ = disabled;
            let client = rpc::client().await?;
            client.user_sessions(username).await.map_err(rpc::error)
        },
    ));
    let current = sessions.read().clone();
    rsx! {
        div { class: "section-head",
            h2 { "Sessions" }
            span { "Revoke signs one device out. To sign {name} out of everything, disable the account." }
        }
        match current {
            None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
            Some(Err(error)) => rsx! {
                LoadError { what: "Couldn't load the sessions", error, on_retry: move |_| sessions.restart() }
            },
            Some(Ok(list)) => rsx! {
                section { class: "table-card",
                    table { class: "table",
                        thead {
                            tr {
                                th { "Device" }
                                th { class: "max-md:hidden", "Kind" }
                                th { "Last used" }
                                th {}
                            }
                        }
                        tbody {
                            if list.is_empty() {
                                tr { td { class: "quiet", colspan: "4", "No live sessions." } }
                            }
                            for s in list {
                                tr { key: "{s.id}",
                                    td {
                                        b { "{session_device(&s.endpoint_id, &s.device_name, s.platform)}" }
                                        if s.current {
                                            span { class: "pill ml-2", "This browser" }
                                        }
                                    }
                                    td { class: "quiet max-md:hidden", "{client_kind(s.endpoint_id.as_deref(), s.platform)}" }
                                    td { class: "quiet", "{ago(now(), s.last_used_at)}" }
                                    td { class: "end",
                                        button {
                                            class: "btn btn-danger",
                                            onclick: {
                                                let username = username.clone();
                                                let id = s.id.clone();
                                                let device = session_device(&s.endpoint_id, &s.device_name, s.platform);
                                                move |_| {
                                                    let username = username.clone();
                                                    let id = id.clone();
                                                    let device = device.clone();
                                                    spawn(async move {
                                                        let ended = async {
                                                            let client = rpc::client().await?;
                                                            client.user_revoke_session(username, id).await.map_err(rpc::error)
                                                        };
                                                        match ended.await {
                                                            Ok(()) => say(toast, true, format!("Signed out of {device}")),
                                                            Err(e) => say(toast, false, e.explain()),
                                                        }
                                                        sessions.restart();
                                                    });
                                                }
                                            },
                                            "Revoke"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
    }
}

/// A change to one of the user's devices, held while the page asks about
/// it or the admin confirms it is them.
#[derive(Debug, Clone, PartialEq)]
struct DeviceChange {
    change: Change,
    endpoint_id: String,
    name: String,
    question: Option<Question>,
}

/// The user's devices (WebUserSessions, WebAdminPhone's "One user's
/// devices"; P4-03): on each, whether their PIN is asked for, whether it
/// is trusted to open their profile without a password, and whether it
/// opens it at start. The PIN is one per user: clearing it is one action
/// for the section, not a row's.
#[component]
fn UserDevices(
    username: String,
    has_pin: bool,
    toast: Signal<Option<Said>>,
    on_clear: EventHandler<()>,
) -> Element {
    let mut devices = use_resource(use_reactive(
        (&username, &has_pin),
        |(username, has_pin)| async move {
            // Read again when the PIN comes or goes: clearing it asks for it
            // nowhere.
            let _ = has_pin;
            let client = rpc::client().await?;
            client.user_devices(username).await.map_err(rpc::error)
        },
    ));
    let whose = Whose::Theirs(username.clone());
    // The change whose question is open.
    let mut pending = use_signal(|| None::<DeviceChange>);
    // A change may need "Confirm it's you" first, then is made again.
    let confirming = use_signal(|| None::<String>);
    let mut again = use_signal(|| None::<DeviceChange>);
    let name = username.clone();
    let make = use_callback(move |change: DeviceChange| {
        let username = name.clone();
        spawn(async move {
            let made = async {
                let client = rpc::client().await?;
                match trust::trust_request(change.change) {
                    Some((trusted, auto_login)) => {
                        client
                            .user_set_device_trust(
                                username.clone(),
                                change.endpoint_id.clone(),
                                trusted,
                                auto_login,
                            )
                            .await
                    }
                    None => {
                        client
                            .user_set_pin_required(
                                username.clone(),
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
                    trust::done(change.change, &change.name, &Whose::Theirs(username)),
                ),
                Err(e) if asks_to_confirm(&e, confirming, trust::about(change.change)) => {
                    again.set(Some(change));
                }
                Err(e) => say(toast, false, e.explain()),
            }
            devices.restart();
        });
    });
    let press = use_callback(
        move |(switch, device, name): (Switch, DeviceEntry, String)| {
            let change = trust::change(switch, &device);
            let asked = DeviceChange {
                change,
                question: trust::question(change, &device, &name, &whose),
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
    let current = devices.read().clone();
    rsx! {
        ConfirmItsYou {
            asking: confirming,
            on_confirmed: move |_| {
                let change = again.write().take();
                if let Some(change) = change {
                    make.call(change);
                }
            },
        }
        if let Some(DeviceChange { question: Some(question), .. }) = pending() {
            TrustDialog {
                question,
                on_yes: move |_| {
                    let change = pending.write().take();
                    if let Some(change) = change {
                        make.call(change);
                    }
                },
                on_no: move |_| pending.set(None),
            }
        }
        div { class: "flex flex-wrap items-end justify-between gap-3",
            div { class: "section-head",
                h2 { "Devices" }
                span { "On each device: whether it asks for the PIN, may open this profile without a password, and opens it at start." }
            }
            if has_pin {
                button { class: "btn", onclick: move |_| on_clear.call(()), "Clear PIN" }
            } else {
                span { class: "text-[13px] text-ctp-subtext0", "No PIN set" }
            }
        }
        match current {
            None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
            Some(Err(error)) => rsx! {
                LoadError { what: "Couldn't load the devices", error, on_retry: move |_| devices.restart() }
            },
            Some(Ok(list)) => rsx! {
                section { class: "table-card",
                    table { class: "table",
                        thead {
                            tr {
                                th { "Device" }
                                th { class: "max-md:hidden", "Kind" }
                                th { "PIN" }
                                th { "Trusted" }
                                th { "Auto-login" }
                            }
                        }
                        tbody {
                            if list.is_empty() {
                                tr { td { class: "quiet", colspan: "5", "No devices yet. A device shows here once {username} signs in on it." } }
                            }
                            for d in list {
                                tr { key: "{d.endpoint_id}",
                                    td {
                                        b { "{session_device(&Some(d.endpoint_id.clone()), &d.name, d.platform)}" }
                                        if d.trusted {
                                            span { class: "pill pill-trusted ml-2", "Trusted" }
                                        }
                                    }
                                    td { class: "quiet max-md:hidden", "{client_kind(Some(&d.endpoint_id), d.platform)}" }
                                    for (switch, what, on) in [
                                        (Switch::Pin, "PIN", d.pin_required),
                                        (Switch::Trusted, "Trusted", d.trusted),
                                        (Switch::AutoLogin, "Auto-login", d.auto_login),
                                    ] {
                                        td { key: "{what}",
                                            button {
                                                class: "switch",
                                                role: "switch",
                                                aria_checked: "{on}",
                                                aria_label: "{what} on this device",
                                                disabled: !trust::can_press(switch, &d, has_pin),
                                                title: device_switch_title(switch, &d, has_pin),
                                                onclick: {
                                                    let device = d.clone();
                                                    let name = session_device(&Some(d.endpoint_id.clone()), &d.name, d.platform);
                                                    move |_| press.call((switch, device.clone(), name.clone()))
                                                },
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
    }
}

/// Why a switch cannot be pressed, as its tooltip.
fn device_switch_title(switch: Switch, device: &DeviceEntry, has_pin: bool) -> &'static str {
    if trust::can_press(switch, device, has_pin) {
        return "";
    }
    match switch {
        Switch::Pin => "There is no PIN to ask for.",
        Switch::AutoLogin => "Trust the device first.",
        Switch::Trusted => "",
    }
}

/// A user's passkeys, for an admin: seen and removed, never added. Only
/// their user can add one. `name` is what the page calls them.
#[component]
fn UserPasskeys(username: String, name: String) -> Element {
    let mut passkeys = use_resource(use_reactive((&username,), |(username,)| async move {
        let client = rpc::client().await?;
        client.user_passkeys(username).await.map_err(rpc::error)
    }));
    let mut notice = use_signal(|| None::<String>);
    // Removing one may need "Confirm it's you" first, then is tried again.
    let confirming = use_signal(|| None::<String>);
    let mut again = use_signal(|| None::<(String, String)>);
    let owner = username.clone();
    let remove = use_callback(move |(id, name): (String, String)| {
        let username = owner.clone();
        spawn(async move {
            let removed = async {
                let client = rpc::client().await?;
                client
                    .user_remove_passkey(username, id.clone())
                    .await
                    .map_err(rpc::error)
            };
            match removed.await {
                Ok(()) => notice.set(Some(format!("Passkey {name} removed."))),
                Err(e) if asks_to_confirm(&e, confirming, "removing a passkey") => {
                    again.set(Some((id, name)));
                }
                Err(e) => notice.set(Some(e.explain())),
            }
            passkeys.restart();
        });
    });
    let current = passkeys.read().clone();
    rsx! {
        ConfirmItsYou {
            asking: confirming,
            on_confirmed: move |_| {
                let pending = again.write().take();
                if let Some(pending) = pending {
                    remove.call(pending);
                }
            },
        }
        section { class: "card",
            div { class: "card-head",
                icons::Key { size: 18 }
                h2 { "{name}'s passkeys" }
                span { class: "text-[13px] text-ctp-subtext0", "Only they can add one." }
            }
            div { class: "card-body",
                if let Some(text) = notice() {
                    p { class: "m-0 text-sm text-ctp-green", role: "status", "{text}" }
                }
                match current {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        p { class: "m-0 text-sm text-ctp-subtext0", "No passkeys." }
                    },
                    Some(Ok(list)) => rsx! {
                        for p in list {
                            div { key: "{p.id}", class: "row",
                                span { class: "row-what",
                                    span { icons::Fingerprint {} b { {crate::logic::passkey_title(&p)} } }
                                    span { class: "row-sub",
                                        {match p.last_used_at {
                                            Some(at) => format!("{} · used {}", p.rp_id, ago(now(), at)),
                                            None => format!("{} · never used", p.rp_id),
                                        }}
                                    }
                                    if !p.usable {
                                        span { class: "row-sub text-ctp-yellow", "Made for {p.rp_id}: it does not work at the server's address now." }
                                    }
                                }
                                span { class: "row-do",
                            button {
                                class: "btn btn-danger",
                                onclick: {
                                    let id = p.id.clone();
                                    let name = p.name.clone();
                                    move |_| remove.call((id.clone(), name.clone()))
                                },
                                icons::Trash {}
                                "Remove"
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

/// The design's WebServer board: this server, Plex, and the media paths
/// check, side by side where there is room.
#[component]
pub fn Server() -> Element {
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            div { class: "page-head hidden md:flex",
                h1 { class: "page-title page-title-admin", "Server" }
            }
            div { class: "server-grid",
                ThisServer {}
                div { class: "flex flex-col gap-4",
                    Plex {}
                    PathCheck {}
                }
            }
        }
    }
}

/// A label and its value, in a card.
#[component]
fn Pair(label: String, children: Element) -> Element {
    rsx! {
        div { class: "pair",
            span { class: "pair-label", "{label}" }
            span { class: "min-w-0", {children} }
        }
    }
}

#[component]
fn ThisServer() -> Element {
    let status = use_resource(|| async {
        let client = rpc::client().await?;
        client.server_status().await.map_err(rpc::error)
    });
    let current = status.read().clone();
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::Server {}
                h2 { "This server" }
            }
            div { class: "card-body",
                match current {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(s)) => rsx! {
                        Pair { label: "Name", b { "{s.name}" } }
                        Pair { label: "Version", span { class: "mono-value", {server_version(&s.version, s.revision.as_deref())} } }
                        Pair { label: "Fingerprint",
                            match s.fingerprint {
                                Some(fingerprint) => rsx! {
                                    div { class: "fingerprint", "{fingerprint}" }
                                    div { class: "mt-1 text-xs text-ctp-subtext0", "Clients show this when they first connect." }
                                },
                                None => rsx! { span { class: "text-ctp-subtext0", "starting…" } },
                            }
                        }
                        Pair { label: "Key",
                            match s.endpoint_id {
                                Some(key) => rsx! {
                                    div { class: "flex flex-col items-start gap-2.5 md:flex-row",
                                        span { class: "mono-value", "{key}" }
                                        CopyButton { text: key }
                                    }
                                },
                                None => rsx! { span { class: "text-ctp-subtext0", "starting…" } },
                            }
                        }
                        // Read only: the config sets it, and the API does not change it.
                        Pair { label: "LAN discovery",
                            if s.mdns {
                                span { class: "state state-ok text-sm", "On" }
                                span { class: "text-[13px] text-ctp-subtext0", " · clients on this network find the server by themselves" }
                            } else {
                                span { class: "state text-sm text-ctp-subtext1", "Off" }
                                span { class: "text-[13px] text-ctp-subtext0", " · clients need its address or key" }
                            }
                        }
                        Pair { label: "Addresses",
                            if s.addresses.is_empty() {
                                span { class: "text-ctp-subtext0", "starting…" }
                            } else {
                                div { class: "flex flex-col gap-1",
                                    for address in s.addresses {
                                        span { key: "{address}", class: "mono-value", "{address}" }
                                    }
                                }
                            }
                        }
                        // The ones a signed-in device is told to dial (PX-12):
                        // `network.announce` leaves out container bridges.
                        Pair { label: "Told to devices",
                            if s.told_addresses.is_empty() {
                                span { class: "text-ctp-subtext0", "none" }
                            } else {
                                div { class: "flex flex-col gap-1",
                                    for address in s.told_addresses {
                                        span { key: "{address}", class: "mono-value", "{address}" }
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

#[component]
fn Plex() -> Element {
    let mut plex = use_resource(|| async {
        let client = rpc::client().await?;
        client.plex_status().await.map_err(rpc::error)
    });
    let current = plex.read().clone();
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::Tv {}
                h2 { "Plex" }
            }
            div { class: "card-body",
                match current {
                    None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(p)) if !p.reachable => rsx! {
                        div { class: "alert alert-error", role: "alert",
                            icons::CircleX {}
                            span { class: "flex flex-col gap-[3px]",
                                b { "Plex cannot be reached" }
                                span { class: "text-ctp-text",
                                    "{p.error.clone().unwrap_or_default()}"
                                    // Most of the server's reasons name the URL already.
                                    if let Some(url) = p.url.clone().filter(|u| !p.error.as_deref().unwrap_or_default().contains(u.as_str())) {
                                        " Tried "
                                        span { class: "mono-value", "{url}" }
                                        "."
                                    }
                                }
                            }
                        }
                        div {
                            button { class: "btn", onclick: move |_| plex.restart(), icons::RotateCcw {} "Try again" }
                        }
                    },
                    Some(Ok(p)) => rsx! {
                        div { class: "row",
                            span { class: "row-what",
                                b { {p.server_name.clone().unwrap_or_else(|| "Plex Media Server".into())} }
                                span { class: "row-sub",
                                    {plex_line(p.version.as_deref(), p.url.as_deref(), p.libraries.len())}
                                }
                            }
                            span { class: "row-do", span { class: "state state-ok", "Reachable" } }
                        }
                    },
                }
            }
        }
    }
}

#[component]
pub(super) fn CopyButton(text: String) -> Element {
    let mut copied = use_signal(|| false);
    rsx! {
        button {
            class: "btn shrink-0",
            onclick: move |_| {
                if let Some(window) = web_sys::window() {
                    let _ = window.navigator().clipboard().write_text(&text);
                    copied.set(true);
                }
            },
            icons::Link {}
            if copied() { "Copied" } else { "Copy" }
        }
    }
}

/// Whether every file Plex lists can be read here (WebServer, and
/// WebServerStates 2 and 3). The check reports nothing until it is done,
/// so "checking" is a spinner, not a count. The server keeps the last one,
/// shown with how long ago it ran (P3-18).
#[component]
fn PathCheck() -> Element {
    let mut result = use_signal(|| None::<Result<PathCheckRun, ApiError>>);
    let mut busy = use_signal(|| false);
    // "checked N minutes ago" moves on while the page is open.
    let mut clock = use_signal(now);
    use_hook(move || {
        spawn(async move {
            let last = async {
                let client = rpc::client().await?;
                client.plex_last_path_check().await.map_err(rpc::error)
            };
            // None before the first check; an error is shown by the run.
            if let Ok(Some(run)) = last.await
                && result.peek().is_none()
            {
                result.set(Some(Ok(run)));
            }
            loop {
                sleep(30_000).await;
                clock.set(now());
            }
        });
    });
    let run = move |_| {
        busy.set(true);
        spawn(async move {
            let checked = async {
                let client = rpc::client().await?;
                client.plex_check_paths(None).await.map_err(rpc::error)
            };
            result.set(Some(checked.await));
            busy.set(false);
        });
    };
    let done = result().is_some();
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::ShieldCheck {}
                h2 { "Media paths" }
            }
            div { class: "card-body",
                p { class: "m-0 text-[13px] text-ctp-subtext0",
                    "Whether this server can read every file Plex lists, through the path mapping."
                }
                if busy() {
                    div { class: "flex items-center gap-2.5 text-sm", role: "status",
                        span { class: "flex animate-spin", icons::Loader {} }
                        "Checking every file…"
                    }
                }
                match result() {
                    None => rsx! {},
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(run)) => rsx! {
                        for (i, r) in run.reports.into_iter().enumerate() {
                            // Said once, on the first row, as the board does:
                            // every row is of the same check.
                            PathRow {
                                key: "{r.library}",
                                report: r,
                                checked: (i == 0).then(|| format!("checked {}", ago(clock(), run.at))),
                            }
                        }
                    },
                }
                div {
                    if done {
                        button { class: "btn", disabled: busy(), onclick: run, icons::RotateCcw {} "Check every file again" }
                    } else {
                        button { class: "btn-dialog btn-dialog-primary h-9", disabled: busy(), onclick: run, "Check every file" }
                    }
                }
            }
        }
    }
}

#[component]
fn PathRow(report: PathReport, checked: Option<String>) -> Element {
    let when = checked.map(|c| format!(" · {c}")).unwrap_or_default();
    rsx! {
        div { class: "row",
            span { class: "row-what",
                b { "{report.library}" }
                match path_state(&report) {
                    PathState::Ok { files } => rsx! {
                        span { class: "row-sub", "{thousands(files)} files{when}" }
                    },
                    PathState::Missing { local_path, .. } => rsx! {
                        span { class: "row-sub", "e.g. " span { class: "mono-value", "{local_path}" } "{when}" }
                    },
                    PathState::NoMapping { plex_path } => rsx! {
                        span { class: "row-sub",
                            "Plex path "
                            span { class: "mono-value", "{plex_path}" }
                            " has no mapping.{when}"
                        }
                    },
                }
            }
            span { class: "row-do",
                match path_state(&report) {
                    PathState::Ok { .. } => rsx! { span { class: "state state-ok", "OK" } },
                    PathState::Missing { count, .. } => rsx! { span { class: "state state-warn", "{thousands(count)} missing" } },
                    PathState::NoMapping { .. } => rsx! { span { class: "state state-bad", "No matching mapping" } },
                }
            }
        }
    }
}

/// The user page's sign-in providers (P3-04), as its Plex account is
/// shown: each account the user linked, with Unlink only. Only the user
/// can link one, in My profile.
#[component]
fn UserSso(username: String, name: String, toast: Signal<Option<Said>>) -> Element {
    let who = username.clone();
    let mut links = use_resource(move || {
        let who = who.clone();
        async move {
            let client = rpc::client().await?;
            client.user_sso_links(who).await.map_err(rpc::error)
        }
    });
    let site = use_resource(super::http::site);
    let mut unlinking = use_signal(|| None::<SsoLink>);
    // Unlinking may need "Confirm it's you" first, then is tried again.
    let confirming = use_signal(|| None::<String>);
    let unlink = {
        let username = username.clone();
        let name = name.clone();
        use_callback(move |link: SsoLink| {
            let username = username.clone();
            let name = name.clone();
            spawn(async move {
                let done = async {
                    let client = rpc::client().await?;
                    client
                        .user_sso_unlink(username, link.issuer.clone())
                        .await
                        .map_err(rpc::error)
                };
                let provider = link.provider.clone().unwrap_or(link.issuer.clone());
                match done.await {
                    Ok(()) => say(toast, true, format!("{name}'s {provider} account unlinked")),
                    // Kept as being unlinked, to try again once confirmed.
                    Err(e) if asks_to_confirm(&e, confirming, "unlinking an account") => return,
                    Err(e) => say(toast, false, e.explain()),
                }
                unlinking.set(None);
                links.restart();
            });
        })
    };
    let providers = site
        .read()
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .map(|s| s.sso.clone())
        .unwrap_or_default();
    let current = links.read().clone();
    let any = matches!(&current, Some(Ok(l)) if !l.is_empty());
    if providers.is_empty() && !any {
        return rsx! {};
    }
    rsx! {
        ConfirmItsYou {
            asking: confirming,
            on_confirmed: move |_| {
                let pending = unlinking();
                if let Some(link) = pending {
                    unlink.call(link);
                }
            },
        }
        div { class: "section-head",
            h2 { "Sign-in providers" }
            span { "Accounts {name} signs in with. Only {name} can link one, in My profile." }
        }
        match current {
            None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
            Some(Err(e)) => rsx! { Alert { text: e.explain() } },
            Some(Ok(list)) if list.is_empty() => rsx! {
                section { class: "card",
                    div { class: "card-body",
                        p { class: "m-0 text-sm text-ctp-subtext0", "None linked." }
                    }
                }
            },
            Some(Ok(list)) => rsx! {
                section { class: "card",
                    div { class: "card-body",
                        for link in list {
                            div { class: "row", key: "{link.issuer}",
                                span { class: "flex min-w-0 items-center gap-3",
                                    span { class: "flex text-ctp-teal", icons::ShieldCheck { size: 18 } }
                                    span { class: "row-what",
                                        b { "{link.provider.clone().unwrap_or(link.issuer.clone())}" }
                                        span { class: "row-sub",
                                            {sso_line(&link)}
                                        }
                                    }
                                }
                                span { class: "row-do",
                                    if unlinking().as_ref().is_some_and(|u| u.issuer == link.issuer) {
                                        span { class: "text-[13px]", "Unlink it?" }
                                        button {
                                            class: "btn btn-danger",
                                            onclick: {
                                                let link = link.clone();
                                                move |_| unlink.call(link.clone())
                                            },
                                            "Unlink"
                                        }
                                        button { class: "btn", onclick: move |_| unlinking.set(None), "Keep" }
                                    } else {
                                        button {
                                            class: "btn btn-danger",
                                            onclick: {
                                                let link = link.clone();
                                                move |_| unlinking.set(Some(link.clone()))
                                            },
                                            icons::Unlink { size: 15 }
                                            "Unlink"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
    }
}

/// "alice@example.org · linked 2026-10-04 · last used 2 hours ago".
fn sso_line(link: &SsoLink) -> String {
    let mut parts = Vec::new();
    if let Some(who) = &link.display {
        parts.push(who.clone());
    }
    parts.push(format!("linked {}", crate::logic::date(link.linked_at)));
    match link.last_used_at {
        Some(at) => parts.push(format!("last used {}", ago(now(), at))),
        None => parts.push("never used".into()),
    }
    if link.provider.is_none() {
        parts.push("provider no longer set up".into());
    }
    parts.join(" · ")
}
