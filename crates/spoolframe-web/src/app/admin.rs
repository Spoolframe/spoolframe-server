//! An admin's pages (P3-02): users, a user's sessions, and the server, as
//! the design's WebUsers, WebUserSessions and WebServer boards lay them
//! out. Everything goes over the admin JSON-RPC API, which the CLI uses
//! too.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, DeviceEntry, NewUser, PathCheckRun, PathReport, Role, SsoLink, UserInfo,
};

use super::profile::{ConfirmItsYou, DevicesLegend, TrustDialog, asks_to_confirm};
use super::{Alert, Field, Route, icons, initial, now, role_name, rpc};
use crate::logic::{
    AddUserError, ApiError, PathState, UserStatus, add_user_error, ago, avatar_class, client_kind,
    count, key_start, last_admin, password_remove_error, path_state, plex_line, server_version,
    shared_devices, sign_in_methods, thousands, user_change_error, user_status, users_in_order,
};
use crate::trust::{self, Change, LegendLine, Question, Switch, Whose};

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
    let me = use_context::<UserInfo>();
    let mut list = use_resource(users);
    let toast = use_signal(|| None::<Said>);
    let mut adding = use_signal(|| false);
    // Whose password is being reset: their username, and the name the
    // dialog calls them by.
    let mut resetting = use_signal(|| None::<(String, String)>);
    // Whose devices the panel under the table shows: the admin's own
    // until another row is picked (WebUsers).
    let mut selected = use_signal(|| me.username.clone());
    let current = list.read().clone().map(|r| r.map(users_in_order));
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
                        selected: selected(),
                        on_select: move |name: String| selected.set(name),
                        on_reset: move |who: (String, String)| resetting.set(Some(who)),
                    }
                    div { class: "flex flex-col gap-3.5 md:hidden",
                        for user in users.iter().cloned() {
                            UserCard { key: "{user.username}", user }
                        }
                    }
                    if users.len() == 1 {
                        OnlyYou { on_add: move |_| adding.set(true) }
                    }
                    if let Some(user) = users.iter().find(|u| u.username == selected()).cloned() {
                        DevicesPanel {
                            key: "{user.username}",
                            user,
                            users: users.clone(),
                            toast,
                            on_changed: move |_| list.restart(),
                        }
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
        if let Some((username, name)) = resetting() {
            ResetPassword {
                username,
                name,
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
/// their page, where everything but a password reset is done; the row, or
/// its device count, shows their devices under the table.
#[component]
fn UserTable(
    users: Vec<UserInfo>,
    selected: String,
    on_select: EventHandler<String>,
    on_reset: EventHandler<(String, String)>,
) -> Element {
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
                        tr {
                            key: "{user.username}",
                            class: match (user.disabled, user.username == selected) {
                                (true, true) => "off opens here",
                                (true, false) => "off opens",
                                (false, true) => "opens here",
                                (false, false) => "opens",
                            },
                            onclick: {
                                let name = user.username.clone();
                                move |_| on_select.call(name.clone())
                            },
                            td {
                                Link { class: "who", to: Route::UserSessions { username: user.username.clone() },
                                    span { class: "{avatar_class(&user)} avatar-md", "{initial(&shown_name(&user))}" }
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
                            td {
                                button {
                                    class: "count-button",
                                    aria_pressed: "{user.username == selected}",
                                    aria_label: "Show {shown_name(&user)}'s devices",
                                    onclick: {
                                        let name = user.username.clone();
                                        move |e: MouseEvent| {
                                            e.stop_propagation();
                                            on_select.call(name.clone());
                                        }
                                    },
                                    "{user.device_count}"
                                }
                            }
                            td { "{user.session_count}" }
                            td { Status { user: user.clone() } }
                            td { class: "end",
                                button {
                                    class: "btn h-8 px-3 font-normal",
                                    onclick: {
                                        let who = (user.username.clone(), shown_name(&user));
                                        move |e: MouseEvent| {
                                            e.stop_propagation();
                                            on_reset.call(who.clone());
                                        }
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

/// Everyone's devices, by the names the page shows them, for "also: …".
async fn everyones_devices(
    users: Vec<UserInfo>,
) -> Result<Vec<(String, Vec<DeviceEntry>)>, ApiError> {
    let client = rpc::client().await?;
    let mut all = Vec::with_capacity(users.len());
    for user in users {
        let devices = client
            .user_devices(user.username.clone())
            .await
            .map_err(rpc::error)?;
        all.push((shown_name(&user), devices));
    }
    Ok(all)
}

/// "Andrey's devices" (WebUsers): a card per device the user signed in
/// on, its key's start and kind, when it was last seen, the user's
/// sessions on it with Revoke, and who else signed in on it.
#[component]
fn DevicesPanel(
    user: UserInfo,
    users: Vec<UserInfo>,
    toast: Signal<Option<Said>>,
    on_changed: EventHandler<()>,
) -> Element {
    let name = shown_name(&user);
    let username = user.username.clone();
    let devices = use_resource(use_reactive((&users,), |(users,)| everyones_devices(users)));
    let who = username.clone();
    let mut sessions = use_resource(move || {
        let who = who.clone();
        async move {
            let client = rpc::client().await?;
            client.user_sessions(who).await.map_err(rpc::error)
        }
    });
    let revoke = {
        let username = username.clone();
        use_callback(move |(id, device): (String, String)| {
            let username = username.clone();
            spawn(async move {
                let ended = async {
                    let client = rpc::client().await?;
                    client
                        .user_revoke_session(username, id)
                        .await
                        .map_err(rpc::error)
                };
                match ended.await {
                    Ok(()) => say(toast, true, format!("Signed out of {device}")),
                    Err(e) => say(toast, false, e.explain()),
                }
                sessions.restart();
                on_changed.call(());
            });
        })
    };
    let found = match (&*devices.read(), &*sessions.read()) {
        (Some(Ok(all)), Some(Ok(live))) => Some(Ok((shared_devices(&name, all), live.clone()))),
        (Some(Err(e)), _) | (_, Some(Err(e))) => Some(Err(e.clone())),
        _ => None,
    };
    rsx! {
        section { class: "card devices-panel hidden md:flex",
            div { class: "card-head",
                h2 { "{name}'s devices" }
                span { class: "ml-auto text-[13px] text-ctp-subtext0", "A session only works from the device key it was created on." }
            }
            match found {
                None => rsx! { div { class: "card-body", p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } } },
                Some(Err(e)) => rsx! { div { class: "card-body", Alert { text: e.explain() } } },
                Some(Ok((shared, _))) if shared.is_empty() => rsx! {
                    div { class: "card-body",
                        p { class: "m-0 text-sm text-ctp-subtext0", "No devices yet. A device shows here once {name} signs in on it." }
                    }
                },
                Some(Ok((shared, live))) => rsx! {
                    div { class: "devices-grid",
                        for d in shared {
                            div { key: "{d.device.endpoint_id}", class: "device-tile",
                                div { class: "flex items-baseline justify-between gap-2",
                                    b { class: "min-w-0 break-words", "{device_name(&d.device)}" }
                                    span { class: "shrink-0 text-xs text-ctp-subtext0", "{ago(now(), d.device.last_seen_at)}" }
                                }
                                span { class: "device-tile-key",
                                    "{key_start(&d.device.endpoint_id)} · {client_kind(Some(&d.device.endpoint_id), d.device.platform)}"
                                }
                                for s in live.iter().filter(|s| s.endpoint_id.as_deref() == Some(d.device.endpoint_id.as_str())).cloned() {
                                    div { key: "{s.id}", class: "device-tile-session",
                                        span { "{name} · since {crate::logic::date(s.created_at)}" }
                                        button {
                                            class: "btn btn-danger h-7 px-2.5 text-xs",
                                            onclick: {
                                                let id = s.id.clone();
                                                let device = device_name(&d.device);
                                                move |_| revoke.call((id.clone(), device.clone()))
                                            },
                                            "Revoke"
                                        }
                                    }
                                }
                                if !d.also.is_empty() {
                                    div { class: "device-tile-also", "also: {d.also.join(\", \")}" }
                                }
                            }
                        }
                    }
                },
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
            span { class: "{avatar_class(&user)} avatar-card", "{initial(&shown_name(&user))}" }
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

/// "Set a new password for <username>" (WebUsersStates, 3), saying what
/// it does to `name`, the name the pages show. `on_done` gets what to
/// say, or `None` when cancelled.
#[component]
fn ResetPassword(
    username: String,
    name: String,
    on_done: EventHandler<Option<(bool, String)>>,
) -> Element {
    let password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let owner = username.clone();
    let attempt = use_callback(move |()| {
        busy.set(true);
        let username = owner.clone();
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
                span { class: "hint", "{name} will be signed out on every device, and needs the new password to sign in again." }
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
    /// Take the user's password away: a passkey or a provider signs them
    /// in from then on.
    RemovePassword,
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
    // The phone bar names the user, not the list they came from.
    let super::BarTitle(mut bar_title) = use_context::<super::BarTitle>();
    let titled = shown_name(&user);
    use_effect(use_reactive((&titled,), move |(titled,)| {
        bar_title.set(Some(titled));
    }));
    let toast = use_signal(|| None::<Said>);
    // Bumped by each change to a device, which may end its sessions.
    let devices_changed = use_signal(|| 0u32);
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
                span { class: "{avatar_class(&user)} avatar-lg", "{initial(&name)}" }
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
                if user.has_password {
                    button { class: "btn", onclick: move |_| asking.set(Some(Ask::RemovePassword)), icons::Trash {} "Remove password" }
                }
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
        UserSessionList { username: username.clone(), name: name.clone(), disabled: user.disabled, toast, devices_changed }
        super::plex::UserPlex { username: username.clone(), name: name.clone(), toast }
        UserSso { username: username.clone(), name: name.clone(), toast }
        super::portable::PortableCard {
            owner: crate::portable::Owner::User { name: name.clone() },
            username: username.clone(),
            tie: user.portable.clone(),
            toast,
            on_severed: move |_| on_changed.call(()),
        }
        UserDevices {
            username: username.clone(),
            name: name.clone(),
            has_pin: user.has_pin,
            toast,
            devices_changed,
            on_clear: move |_| asking.set(Some(Ask::ClearPin)),
        }
        UserPasskeys { username: username.clone(), name: name.clone() }
        if resetting() {
            ResetPassword {
                username: username.clone(),
                name: name.clone(),
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
        Ask::RemovePassword => (
            format!("Remove {username}'s password?"),
            format!(
                "{username} signs in with their passkey or sign-in provider from then on. Their sessions end, and their trusted devices are forgotten."
            ),
            "Remove password",
        ),
    };
    let confirming = use_signal(|| None::<String>);
    let about = match ask {
        Ask::Role(_) => "making an admin",
        Ask::RemovePassword => "removing a password",
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
                    Ask::RemovePassword => {
                        client
                            .user_remove_password(username.clone())
                            .await
                            .map_err(rpc::error)?;
                        Ok(format!("{username}'s password removed"))
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
                Err(e) if ask == Ask::RemovePassword => {
                    (false, password_remove_error(&e, Some(&username)))
                }
                Err(e) => (false, user_change_error(&e, &username)),
            }));
        });
    });
    let danger = matches!(ask, Ask::Disable | Ask::RemovePassword);
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: if danger { "dialog-head dialog-head-danger" } else { "dialog-head" },
                    match ask {
                        Ask::Disable => rsx! { icons::CircleX { size: 22 } },
                        Ask::ClearPin => rsx! { icons::Lock { size: 22 } },
                        Ask::RemovePassword => rsx! { icons::KeyRound { size: 22 } },
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

/// What a session's device is called: its name, its key's start, or, for
/// an admin API session, what signed it in (P3-18), by the same words My
/// profile uses (logic::session_name).
fn session_device(
    endpoint_id: &Option<String>,
    name: &Option<String>,
    platform: Option<spoolframe_admin_api::Platform>,
) -> String {
    match (endpoint_id, name) {
        (Some(id), None) => format!("Device {}", &id[..id.len().min(10)]),
        _ => crate::logic::session_name(
            endpoint_id.as_deref(),
            name.as_deref(),
            platform,
            false,
            None,
        ),
    }
}

/// A session's name on a user's page: this browser's own by its own
/// name, as My profile calls it.
fn session_title(s: &spoolframe_admin_api::SessionEntry) -> String {
    if s.current {
        crate::logic::session_name(
            s.endpoint_id.as_deref(),
            s.device_name.as_deref(),
            s.platform,
            true,
            super::profile::user_agent().as_deref(),
        )
    } else {
        session_device(&s.endpoint_id, &s.device_name, s.platform)
    }
}

#[component]
fn UserSessionList(
    username: String,
    name: String,
    disabled: bool,
    toast: Signal<Option<Said>>,
    devices_changed: Signal<u32>,
) -> Element {
    let mut sessions = use_resource(use_reactive(
        (&username, &disabled),
        move |(username, disabled)| async move {
            // Read again when the account is disabled, or a device's trust
            // changes: no longer trusting a device ends its sessions.
            let _ = (disabled, devices_changed());
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
                                        b { "{session_title(&s)}" }
                                        if s.current {
                                            span { class: "pill ml-2", "This browser" }
                                        }
                                    }
                                    td { class: "quiet max-md:hidden", "{crate::logic::session_kind(s.endpoint_id.as_deref(), s.platform)}" }
                                    td { class: "quiet", "{ago(now(), s.last_used_at)}" }
                                    td { class: "end",
                                        button {
                                            class: "btn btn-danger",
                                            onclick: {
                                                let username = username.clone();
                                                let id = s.id.clone();
                                                let device = session_title(&s);
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
    name: String,
    has_pin: bool,
    toast: Signal<Option<Said>>,
    devices_changed: Signal<u32>,
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
    // The dialogs and toasts call them by the name the page shows.
    let whose = Whose::Theirs(name.clone());
    let legend_whose = whose.clone();
    let told = whose.clone();
    // The change whose question is open.
    let mut pending = use_signal(|| None::<DeviceChange>);
    // A change may need "Confirm it's you" first, then is made again.
    let confirming = use_signal(|| None::<String>);
    let mut again = use_signal(|| None::<DeviceChange>);
    let owner = username.clone();
    let make = use_callback(move |change: DeviceChange| {
        let username = owner.clone();
        let told = told.clone();
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
                Ok(()) => say(toast, true, trust::done(change.change, &change.name, &told)),
                Err(e) if asks_to_confirm(&e, confirming, &trust::about(&told)) => {
                    again.set(Some(change));
                }
                Err(e) => say(toast, false, e.explain()),
            }
            devices.restart();
            let mut devices_changed = devices_changed;
            devices_changed += 1;
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
    // The device whose "Delete …?" is open, by endpoint id and name; the
    // one deleted again once "Confirm it's you" is done.
    let mut deleting = use_signal(|| None::<(String, String)>);
    let confirming_delete = use_signal(|| None::<String>);
    let mut delete_again = use_signal(|| None::<(String, String)>);
    let deleter = username.clone();
    let delete_whose = Whose::Theirs(name.clone());
    let delete = use_callback(move |(endpoint_id, device): (String, String)| {
        let username = deleter.clone();
        let whose = delete_whose.clone();
        spawn(async move {
            let deleted = async {
                let client = rpc::client().await?;
                client
                    .user_forget_device(username, endpoint_id.clone())
                    .await
                    .map_err(rpc::error)
            };
            match deleted.await {
                Ok(()) => say(toast, true, trust::forgotten(&device)),
                Err(e) if asks_to_confirm(&e, confirming_delete, &trust::forget_about(&whose)) => {
                    delete_again.set(Some((endpoint_id, device)));
                }
                Err(e) => say(toast, false, e.explain()),
            }
            devices.restart();
            let mut devices_changed = devices_changed;
            devices_changed += 1;
        });
    });
    let asked_whose = Whose::Theirs(name.clone());
    let current = devices.read().clone();
    rsx! {
        if let Some((endpoint_id, device)) = deleting() {
            TrustDialog {
                question: trust::forget_question(&device, &asked_whose),
                on_yes: move |_| {
                    deleting.set(None);
                    delete.call((endpoint_id.clone(), device.clone()));
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
        div { class: "section-head",
            h2 { "Devices" }
            span {
                "Turning trust off also ends {name}'s sessions on that device. Changes ask you to confirm it's you if you signed in over 10 minutes ago."
            }
        }
        match current {
            None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
            Some(Err(error)) => rsx! {
                LoadError { what: "Couldn't load the devices", error, on_retry: move |_| devices.restart() }
            },
            Some(Ok(list)) if list.is_empty() => rsx! {
                section { class: "card",
                    div { class: "card-body",
                        p { class: "m-0 text-sm text-ctp-subtext0", "No devices yet. A device shows here once {name} signs in on it." }
                    }
                }
            },
            Some(Ok(list)) => rsx! {
                // Wide, the board's table; on a phone, a card per device
                // (WebAdminPhone 5).
                section { class: "table-card max-md:hidden",
                    table { class: "table",
                        thead {
                            tr {
                                th { "Device" }
                                th { "Kind" }
                                for line in trust::legend(&legend_whose, has_pin) {
                                    th {
                                        key: "{line.term}",
                                        title: "{line.text}",
                                        aria_describedby: trust::legend_id(line.switch),
                                        "{line.term}"
                                    }
                                }
                                th {}
                            }
                        }
                        tbody {
                            for d in list.iter().cloned() {
                                tr { key: "{d.endpoint_id}",
                                    td { b { "{device_name(&d)}" } }
                                    td { class: "quiet", "{client_kind(Some(&d.endpoint_id), d.platform)}" }
                                    DeviceSwitches { device: d.clone(), has_pin, cells: true, on_press: press }
                                    td { class: "end",
                                        span { class: "inline-flex items-center gap-2",
                                            ClearPinButton { has_pin, name: name.clone(), on_clear }
                                            DeleteButton {
                                                device: device_name(&d),
                                                on_delete: move |asked| deleting.set(Some(asked)),
                                                endpoint_id: d.endpoint_id.clone(),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "flex flex-col gap-2.5 md:hidden",
                    for d in list {
                        div { key: "{d.endpoint_id}", class: "device-card",
                            div { class: "device-card-head",
                                span { class: "row-what",
                                    b { "{device_name(&d)}" }
                                    span { class: "row-sub", "{client_kind(Some(&d.endpoint_id), d.platform)}" }
                                }
                                DeleteButton {
                                    device: device_name(&d),
                                    on_delete: move |asked| deleting.set(Some(asked)),
                                    endpoint_id: d.endpoint_id.clone(),
                                }
                            }
                            span { class: "device-switches",
                                DeviceSwitches { device: d.clone(), has_pin, cells: false, on_press: press }
                            }
                        }
                    }
                    if has_pin {
                        div { ClearPinButton { has_pin, name: name.clone(), on_clear } }
                    }
                }
                // Under the table or the cards, what each column does.
                DevicesLegend { whose: legend_whose.clone(), has_pin, browsers: false }
            },
        }
    }
}

/// What the user's device is called on their page: its name, or its key's
/// start.
fn device_name(device: &DeviceEntry) -> String {
    session_device(
        &Some(device.endpoint_id.clone()),
        &device.name,
        device.platform,
    )
}

/// "Clear PIN" (WebUserSessions): the PIN is one per user, so each row's
/// button clears it for every device, as its dialog says.
#[component]
fn ClearPinButton(has_pin: bool, name: String, on_clear: EventHandler<()>) -> Element {
    rsx! {
        button {
            class: "btn",
            disabled: !has_pin,
            title: if has_pin { "Clears {name}'s PIN on every device." } else { "{name} has no PIN." },
            onclick: move |_| on_clear.call(()),
            "Clear PIN"
        }
    }
}

/// "Delete" (canvas v75): red words and a trash can, no border, quieter
/// than the buttons beside it. Asks first, by `device`'s name.
#[component]
fn DeleteButton(
    endpoint_id: String,
    device: String,
    on_delete: EventHandler<(String, String)>,
) -> Element {
    rsx! {
        button {
            class: "btn-delete",
            aria_label: "Delete {device}",
            onclick: move |_| on_delete.call((endpoint_id.clone(), device.clone())),
            icons::Trash { size: 14 }
            "Delete"
        }
    }
}

/// A device's three switches: table cells wide, labelled columns in a
/// card on a phone.
#[component]
fn DeviceSwitches(
    device: DeviceEntry,
    has_pin: bool,
    cells: bool,
    on_press: Callback<(Switch, DeviceEntry, String)>,
) -> Element {
    let name = device_name(&device);
    // Only the copy's wording is read here: an admin's legend says "this
    // user's", whoever they are.
    let legend = trust::legend(&Whose::Theirs(String::new()), has_pin);
    rsx! {
        for (LegendLine { switch, term: what, text }, on) in legend.into_iter().zip([
            device.pin_required,
            device.trusted,
            device.auto_login,
        ]) {
            {
                let toggle = rsx! {
                    button {
                        class: "switch",
                        r#type: "button",
                        role: "switch",
                        aria_checked: "{on}",
                        aria_label: "{what} on {name}",
                        aria_describedby: trust::legend_id(switch),
                        disabled: !trust::can_press(switch, &device, has_pin),
                        title: device_switch_title(switch, &device, has_pin),
                        onclick: {
                            let device = device.clone();
                            let name = name.clone();
                            move |_| on_press.call((switch, device.clone(), name.clone()))
                        },
                    }
                };
                if cells {
                    rsx! { td { key: "{what}", {toggle} } }
                } else {
                    rsx! {
                        span { key: "{what}", class: "device-switch",
                            {toggle}
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
            }
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
                                    if let Some(note) = crate::logic::unusable_note(&p, "the server's") {
                                        span { class: "row-sub text-ctp-yellow", "{note}" }
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
            super::restart::PendingRestart {}
            // The title only on a wide screen (WebAdminPhone 3); the
            // restart everywhere, for an admin, whatever is pending.
            div { class: "page-head",
                h1 { class: "page-title page-title-admin max-md:hidden", "Server" }
                super::restart::RestartButton { label: "Restart server", class: "btn" }
            }
            div { class: "server-grid",
                ThisServer {}
                div { class: "flex flex-col gap-4",
                    Plex {}
                    PathCheck {}
                }
            }
            // Under "This server"'s encode budget, the whole width: the
            // cards it is spent on, and where each conversion goes.
            super::gpus::Gpus {}
        }
    }
}

/// A label and its value, in a card. `class`: `pair-stack` puts the label
/// over the value on a phone; `max-md:hidden` leaves the row out there
/// (WebAdminPhone 3 shows Name, Version and the fingerprint only).
#[component]
fn Pair(label: String, #[props(default)] class: String, children: Element) -> Element {
    rsx! {
        div { class: "pair {class}",
            span { class: "pair-label", "{label}" }
            span { class: "min-w-0", {children} }
        }
    }
}

/// Addresses one per line, an IPv6 one wrapping only between its groups.
#[component]
fn Addresses(addresses: Vec<String>) -> Element {
    rsx! {
        div { class: "flex flex-col gap-1",
            for address in addresses {
                span { key: "{address}", class: "mono-value address",
                    {
                        let (wraps, kept) = crate::logic::address_parts(&address);
                        rsx! {
                            span { "{wraps}" }
                            span { class: "whitespace-nowrap", "{kept}" }
                        }
                    }
                }
            }
        }
    }
}

/// The public addresses (`network.wan`) and their sources, or that none
/// was found, with what went wrong finding them.
#[component]
fn PublicAddresses(
    addresses: Vec<spoolframe_admin_api::PublicAddress>,
    notes: Vec<String>,
) -> Element {
    rsx! {
        div { class: "flex flex-col gap-1",
            if addresses.is_empty() {
                span { class: "text-ctp-subtext0", "No public address found." }
            }
            for found in addresses {
                div { key: "{found.address}", class: "flex items-start gap-2.5",
                    span { class: "mono-value address",
                        {
                            let (wraps, kept) = crate::logic::address_parts(&found.address);
                            rsx! {
                                span { "{wraps}" }
                                span { class: "whitespace-nowrap", "{kept}" }
                            }
                        }
                    }
                    span { class: "text-xs text-ctp-subtext0", "{found.source.label()}" }
                }
            }
            for note in notes {
                span { key: "{note}", class: "text-xs text-ctp-yellow", "{note}" }
            }
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
                        Pair { label: "Fingerprint", class: "pair-stack",
                            match s.fingerprint {
                                Some(fingerprint) => rsx! {
                                    div { class: "fingerprint", "{fingerprint}" }
                                    div { class: "mt-1 text-xs text-ctp-subtext0 max-md:hidden", "Clients show this when they first connect." }
                                },
                                None => rsx! { span { class: "text-ctp-subtext0", "starting…" } },
                            }
                        }
                        // The apps' passkeys for this server (PX-14): what a
                        // password manager lists them under.
                        Pair { label: "Passkey domain", class: "pair-stack",
                            {
                                let domain = crate::logic::passkey_domain(&s.server_id);
                                rsx! {
                                    div { class: "flex items-start gap-2.5",
                                        span { class: "mono-value min-w-0", "{domain}" }
                                        CopyButton { text: domain }
                                    }
                                    div { class: "mt-1 text-xs text-ctp-subtext0 max-md:hidden", "Passkeys made in the apps are saved for this domain." }
                                }
                            }
                        }
                        // On a phone the key itself is too long to read:
                        // the board offers it to copy only.
                        if let Some(key) = s.endpoint_id.clone() {
                            div { class: "md:hidden",
                                CopyButton { text: key, label: "Copy key", wide: true }
                            }
                        }
                        Pair { label: "Key", class: "max-md:hidden",
                            match s.endpoint_id {
                                Some(key) => rsx! {
                                    div { class: "flex items-start gap-2.5",
                                        span { class: "mono-value min-w-0", "{key}" }
                                        CopyButton { text: key }
                                    }
                                },
                                None => rsx! { span { class: "text-ctp-subtext0", "starting…" } },
                            }
                        }
                        // `transcode.max_jobs` and the caps beside it: what
                        // the conversions running take now.
                        if let Some(budget) = s.transcoding.budget {
                            Pair { label: "Encode budget", class: "max-md:hidden",
                                div { class: "flex flex-col gap-1",
                                    span { "{budget.used_of_max()} used" }
                                    span { class: "text-xs text-ctp-subtext0",
                                        "Remuxes {budget.remuxes} of {budget.max_remux} · GPU jobs {budget.gpu_jobs} of {budget.max_gpu_jobs}"
                                    }
                                }
                            }
                        }
                        Pair { label: "LAN discovery", class: "max-md:hidden",
                            LanDiscovery { running: s.mdns }
                        }
                        Pair { label: "Addresses", class: "max-md:hidden",
                            if s.addresses.is_empty() {
                                span { class: "text-ctp-subtext0", "starting…" }
                            } else {
                                Addresses { addresses: s.addresses }
                            }
                        }
                        // The ones a signed-in device is told to dial (PX-12):
                        // `network.announce` leaves out container bridges.
                        Pair { label: "Told to devices", class: "max-md:hidden",
                            if s.told_addresses.is_empty() {
                                span { class: "text-ctp-subtext0", "none" }
                            } else {
                                Addresses { addresses: s.told_addresses }
                                div { class: "mt-1 text-xs text-ctp-subtext0", "What a signed-in device is told to dial." }
                            }
                        }
                        // `network.wan` (P6-02): what a device off the LAN
                        // dials, each with where it came from.
                        if s.wan {
                            Pair { label: "Public addresses", class: "max-md:hidden",
                                PublicAddresses { addresses: s.public_addresses, notes: s.public_notes }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// LAN discovery (`network.mdns`) as a switch (WebServer). The setting
/// applies at the next start: the server announces itself, or stops, only
/// then, so a change says so and offers the restart where the server can.
/// `running`: what the server does now.
#[component]
fn LanDiscovery(running: bool) -> Element {
    let mut settings = use_resource(super::wizard::load_settings);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // `network.*` (finding 61): from an old session, "Confirm it's you"
    // first, then the same flip again (`to`, kept for it).
    let confirming = use_signal(|| None::<String>);
    let mut to = use_signal(|| false);
    let flip = use_callback(move |wanted: bool| {
        to.set(wanted);
        busy.set(true);
        spawn(async move {
            let set = async {
                let client = rpc::client().await?;
                client
                    .setting_set("network.mdns".into(), serde_json::Value::Bool(wanted))
                    .await
                    .map_err(rpc::error)
            };
            match set.await {
                Ok(_) => error.set(None),
                Err(e) if asks_to_confirm(&e, confirming, "changing LAN discovery") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
            settings.restart();
        });
    });
    let current = settings.read().clone();
    let Some(Ok(loaded)) = current else {
        // Until the settings load (or if they cannot), what runs now.
        return rsx! { LanLine { on: running } };
    };
    let entry = super::wizard::entry(&loaded, "network.mdns").cloned();
    let wanted = entry
        .as_ref()
        .and_then(|e| e.value.as_ref())
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(running);
    let locked = entry.as_ref().is_some_and(|e| e.locked);
    rsx! {
        div { class: "flex flex-col gap-2",
            div { class: "flex items-start gap-3",
                button {
                    class: "switch shrink-0",
                    r#type: "button",
                    role: "switch",
                    aria_checked: "{wanted}",
                    aria_label: "LAN discovery",
                    disabled: busy() || locked,
                    title: if locked { "Set by the server's config file." } else { "" },
                    onclick: move |_| flip.call(!wanted),
                }
                LanLine { on: wanted }
            }
            super::wizard::Note { settings: loaded.clone(), setting: "network.mdns", on_changed: move |_| settings.restart() }
            if let Some(text) = error() {
                Alert { text }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| flip.call(to()) }
    }
}

/// What LAN discovery being `on` means for clients.
#[component]
fn LanLine(on: bool) -> Element {
    rsx! {
        span { class: "text-[13px] leading-snug text-ctp-subtext1",
            if on {
                "Clients on this network find the server by themselves"
            } else {
                "Clients need its address or key"
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

/// Copies `text`. `label`: what it says before it is pressed; `wide`:
/// the card's width, as the phone board's "Copy key".
#[component]
pub(super) fn CopyButton(
    text: String,
    #[props(default = "Copy".to_owned())] label: String,
    #[props(default)] wide: bool,
) -> Element {
    let mut copied = use_signal(|| false);
    rsx! {
        button {
            class: if wide { "btn w-full justify-center" } else { "btn shrink-0" },
            onclick: move |_| {
                if let Some(window) = web_sys::window() {
                    let _ = window.navigator().clipboard().write_text(&text);
                    copied.set(true);
                }
            },
            icons::Link {}
            if copied() { "Copied" } else { "{label}" }
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
