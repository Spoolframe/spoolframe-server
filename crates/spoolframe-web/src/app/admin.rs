//! An admin's pages (P3-02): users, a user's sessions, and the server.
//! Everything goes over the admin JSON-RPC API, which the CLI uses too.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, NewUser, PathReport, Role, UserInfo};

use super::rpc;
use super::{Alert, Field, Route, Submit};
use crate::logic::{ApiError, ago, last_admin};

/// Now, in Unix seconds, by the browser's clock.
fn now() -> i64 {
    (js_sys::Date::now() / 1000.0) as i64
}

const CARD: &str = "rounded-lg bg-ctp-mantle p-4";
const BUTTON: &str = "rounded-md border border-ctp-surface1 px-2 py-1 text-sm hover:bg-ctp-surface0 disabled:cursor-not-allowed disabled:opacity-40";
const DANGER: &str = "rounded-md border border-ctp-red/50 px-2 py-1 text-sm text-ctp-red hover:bg-ctp-red/10 disabled:cursor-not-allowed disabled:opacity-40";

/// The admin pages' frame: refused to anyone not an admin, as the methods
/// behind it are; tabs for the pages.
#[component]
pub fn AdminFrame() -> Element {
    let user = use_context::<UserInfo>();
    if user.role != Role::Admin {
        return rsx! {
            Alert { text: "Only an admin can see this page." }
        };
    }
    let route = use_route::<Route>();
    let tab = |to: Route, label: &'static str| {
        let here = match (&route, &to) {
            (Route::UserSessions { .. }, Route::Users {}) => true,
            (a, b) => a == b,
        };
        let class = if here {
            "border-b-2 border-ctp-mauve px-3 py-2 font-medium"
        } else {
            "border-b-2 border-transparent px-3 py-2 text-ctp-subtext0 hover:text-ctp-text"
        };
        rsx! {
            Link { class, to, "{label}" }
        }
    };
    rsx! {
        nav { class: "mb-4 flex gap-2 border-b border-ctp-surface0",
            {tab(Route::Users {}, "Users")}
            {tab(Route::Server {}, "Server")}
        }
        Outlet::<Route> {}
    }
}

#[component]
pub fn AdminHome() -> Element {
    navigator().replace(Route::Users {});
    rsx! {}
}

async fn users() -> Result<Vec<UserInfo>, ApiError> {
    let client = rpc::client().await?;
    client.user_list().await.map_err(rpc::error)
}

#[component]
pub fn Users() -> Element {
    let mut list = use_resource(users);
    let mut notice = use_signal(|| None::<String>);
    // Run `action`, then show what it said and read the list again.
    let act = move |action: Action| {
        spawn(async move {
            match action.await {
                Ok(done) => notice.set(Some(done)),
                Err(e) => notice.set(Some(e.explain())),
            }
            list.restart();
        });
    };
    let current = list.read().clone();
    rsx! {
        div { class: "space-y-4",
            if let Some(text) = notice() {
                p { class: "rounded-md bg-ctp-surface0 px-3 py-2 text-sm", role: "status", "{text}" }
            }
            AddUser { on_added: move |name: String| {
                notice.set(Some(format!("Added {name}.")));
                list.restart();
            } }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(users)) => rsx! {
                    div { class: "overflow-x-auto {CARD}",
                        table { class: "w-full text-left text-sm",
                            thead { class: "text-ctp-subtext0",
                                tr {
                                    th { class: "py-2 pr-4", "User" }
                                    th { class: "py-2 pr-4", "Role" }
                                    th { class: "py-2 pr-4", "Last seen" }
                                    th { class: "py-2", "" }
                                }
                            }
                            tbody {
                                for user in users.iter().cloned() {
                                    UserRow {
                                        key: "{user.username}",
                                        last: last_admin(&users, &user.username),
                                        user,
                                        act,
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

type Action = std::pin::Pin<Box<dyn Future<Output = Result<String, ApiError>>>>;

#[component]
fn UserRow(user: UserInfo, last: bool, act: EventHandler<Action>) -> Element {
    let mut resetting = use_signal(|| false);
    let name = user.username.clone();
    let shown = user.display_name.clone().unwrap_or_else(|| name.clone());
    let why_not = "The last admin: make someone else an admin first.";
    let role_button = {
        let name = name.clone();
        let (to, label) = match user.role {
            Role::Admin => (Role::Consumer, "Make consumer"),
            _ => (Role::Admin, "Make admin"),
        };
        let blocked = last && to == Role::Consumer;
        rsx! {
            button {
                class: BUTTON,
                disabled: blocked,
                title: if blocked { why_not } else { "" },
                onclick: move |_| {
                    let name = name.clone();
                    act.call(Box::pin(async move {
                        let client = rpc::client().await?;
                        let user = client.user_set_role(name, to).await.map_err(rpc::error)?;
                        Ok(format!("{} is now {}.", user.username, user.role))
                    }));
                },
                "{label}"
            }
        }
    };
    let enable_button = {
        let name = name.clone();
        if user.disabled {
            rsx! {
                button {
                    class: BUTTON,
                    onclick: move |_| {
                        let name = name.clone();
                        act.call(Box::pin(async move {
                            let client = rpc::client().await?;
                            client.user_enable(name.clone()).await.map_err(rpc::error)?;
                            Ok(format!("{name} can sign in again."))
                        }));
                    },
                    "Enable"
                }
            }
        } else {
            rsx! {
                button {
                    class: DANGER,
                    disabled: last,
                    title: if last { why_not } else { "" },
                    onclick: move |_| {
                        let name = name.clone();
                        act.call(Box::pin(async move {
                            let client = rpc::client().await?;
                            let ended = client.user_disable(name.clone()).await.map_err(rpc::error)?;
                            Ok(format!("{name} is disabled; {ended} session(s) ended."))
                        }));
                    },
                    "Disable"
                }
            }
        }
    };
    rsx! {
        tr { class: "border-t border-ctp-surface0 align-top",
            td { class: "py-2 pr-4",
                div { class: if user.disabled { "text-ctp-subtext0 line-through" } else { "" }, "{shown}" }
                if user.display_name.is_some() {
                    div { class: "text-xs text-ctp-subtext0", "{name}" }
                }
                if user.disabled {
                    div { class: "text-xs text-ctp-red", "disabled" }
                }
            }
            td { class: "py-2 pr-4",
                span { class: if user.role == Role::Admin { "rounded bg-ctp-mauve/20 px-2 py-0.5 text-ctp-mauve" } else { "rounded bg-ctp-surface0 px-2 py-0.5" },
                    "{user.role}"
                }
            }
            td { class: "py-2 pr-4 text-ctp-subtext1",
                {user.last_seen_at.map(|at| ago(now(), at)).unwrap_or_else(|| "never".into())}
            }
            td { class: "py-2",
                div { class: "flex flex-wrap justify-end gap-2",
                    Link { class: BUTTON, to: Route::UserSessions { username: name.clone() }, "Sessions" }
                    button { class: BUTTON, onclick: move |_| resetting.toggle(), "Reset password" }
                    {role_button}
                    {enable_button}
                }
                if resetting() {
                    ResetPassword { username: name.clone(), act, on_done: move |_| resetting.set(false) }
                }
            }
        }
    }
}

#[component]
fn ResetPassword(
    username: String,
    act: EventHandler<Action>,
    on_done: EventHandler<()>,
) -> Element {
    let password = use_signal(String::new);
    let submit = move |e: FormEvent| {
        e.prevent_default();
        let username = username.clone();
        let password = password();
        act.call(Box::pin(async move {
            let client = rpc::client().await?;
            client
                .user_set_password(spoolframe_admin_api::Credentials {
                    username: username.clone(),
                    password,
                })
                .await
                .map_err(rpc::error)?;
            Ok(format!("{username}'s password is changed."))
        }));
        on_done.call(());
    };
    rsx! {
        form { class: "mt-2 flex items-end justify-end gap-2", onsubmit: submit,
            Field { label: "New password", value: password, kind: "password", autocomplete: "new-password" }
            button { class: BUTTON, r#type: "submit", "Set" }
        }
    }
}

#[component]
fn AddUser(on_added: EventHandler<String>) -> Element {
    let mut open = use_signal(|| false);
    let username = use_signal(String::new);
    let display = use_signal(String::new);
    let password = use_signal(String::new);
    let mut admin = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    if !open() {
        return rsx! {
            button { class: BUTTON, onclick: move |_| open.set(true), "Add a user" }
        };
    }
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        spawn(async move {
            let display = display();
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
                Ok(user) => {
                    open.set(false);
                    error.set(None);
                    on_added.call(user.username);
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    rsx! {
        form { class: "space-y-3 {CARD} max-w-md", onsubmit: submit,
            h2 { class: "font-semibold", "Add a user" }
            Field { label: "Username", value: username, autocomplete: "off" }
            Field { label: "Display name (optional)", value: display, autocomplete: "off" }
            Field { label: "Password", value: password, kind: "password", autocomplete: "new-password" }
            label { class: "flex items-center gap-2 text-sm",
                input { r#type: "checkbox", checked: admin(), onchange: move |e| admin.set(e.checked()) }
                "Admin"
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "flex gap-2",
                div { class: "w-32", Submit { text: "Add", busy: busy() } }
                button { class: BUTTON, r#type: "button", onclick: move |_| open.set(false), "Cancel" }
            }
        }
    }
}

#[component]
pub fn UserSessions(username: String) -> Element {
    let name = username.clone();
    let mut sessions = use_resource(move || {
        let name = name.clone();
        async move {
            let client = rpc::client().await?;
            client.user_sessions(name).await.map_err(rpc::error)
        }
    });
    let mut notice = use_signal(|| None::<String>);
    let current = sessions.read().clone();
    rsx! {
        div { class: "space-y-4",
            div { class: "flex items-center gap-3",
                Link { class: BUTTON, to: Route::Users {}, "← Users" }
                h1 { class: "text-lg font-semibold", "{username}'s sessions" }
            }
            if let Some(text) = notice() {
                p { class: "rounded-md bg-ctp-surface0 px-3 py-2 text-sm", role: "status", "{text}" }
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "text-ctp-subtext0", "No live sessions." }
                },
                Some(Ok(list)) => rsx! {
                    ul { class: "divide-y divide-ctp-surface0 {CARD}",
                        for s in list {
                            li { key: "{s.id}", class: "flex items-center justify-between gap-4 py-2",
                                div {
                                    div {
                                        {match (&s.endpoint_id, &s.device_name) {
                                            (Some(_), Some(device)) => device.clone(),
                                            (Some(id), None) => format!("Device {}", &id[..id.len().min(10)]),
                                            (None, _) => "Web or CLI".to_owned(),
                                        }}
                                        if s.current {
                                            span { class: "ml-2 rounded bg-ctp-surface0 px-2 py-0.5 text-xs", "this browser" }
                                        }
                                    }
                                    div { class: "text-xs text-ctp-subtext0",
                                        "signed in {ago(now(), s.created_at)}, last used {ago(now(), s.last_used_at)}"
                                    }
                                }
                                button {
                                    class: DANGER,
                                    onclick: {
                                        let username = username.clone();
                                        let id = s.id.clone();
                                        move |_| {
                                            let username = username.clone();
                                            let id = id.clone();
                                            spawn(async move {
                                                let ended = async {
                                                    let client = rpc::client().await?;
                                                    client.user_revoke_session(username, id).await.map_err(rpc::error)
                                                };
                                                match ended.await {
                                                    Ok(()) => notice.set(Some("Session ended.".into())),
                                                    Err(e) => notice.set(Some(e.explain())),
                                                }
                                                sessions.restart();
                                            });
                                        }
                                    },
                                    "End"
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

#[component]
pub fn Server() -> Element {
    let status = use_resource(|| async {
        let client = rpc::client().await?;
        client.server_status().await.map_err(rpc::error)
    });
    let plex = use_resource(|| async {
        let client = rpc::client().await?;
        client.plex_status().await.map_err(rpc::error)
    });
    let status = status.read().clone();
    let plex = plex.read().clone();
    rsx! {
        div { class: "space-y-4",
            section { class: CARD,
                h2 { class: "mb-3 font-semibold", "Server" }
                match status {
                    None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(s)) => rsx! {
                        dl { class: "grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2 text-sm",
                            dt { class: "text-ctp-subtext0", "Name" }
                            dd { "{s.name}" }
                            dt { class: "text-ctp-subtext0", "Version" }
                            dd { "{s.version}" }
                            dt { class: "text-ctp-subtext0", "Fingerprint" }
                            dd { class: "font-mono", {s.fingerprint.clone().unwrap_or_else(|| "starting…".into())} }
                            dt { class: "text-ctp-subtext0", "Key" }
                            dd { class: "flex items-start gap-2",
                                span { class: "font-mono break-all", {s.endpoint_id.clone().unwrap_or_else(|| "starting…".into())} }
                                if let Some(key) = s.endpoint_id.clone() {
                                    CopyButton { text: key }
                                }
                            }
                            dt { class: "text-ctp-subtext0", "LAN discovery" }
                            dd { if s.mdns { "on (mDNS)" } else { "off" } }
                            dt { class: "text-ctp-subtext0", "Addresses" }
                            dd { class: "font-mono", {if s.addresses.is_empty() { "starting…".to_owned() } else { s.addresses.join(", ") }} }
                        }
                    },
                }
            }
            section { class: CARD,
                h2 { class: "mb-3 font-semibold", "Plex" }
                match plex {
                    None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(p)) if !p.reachable => rsx! {
                        Alert { text: format!("Plex cannot be reached: {}", p.error.unwrap_or_default()) }
                    },
                    Some(Ok(p)) => rsx! {
                        p { class: "mb-2 text-sm",
                            span { class: "text-ctp-green", "● " }
                            {p.server_name.unwrap_or_else(|| "Reachable".into())}
                        }
                        ul { class: "text-sm",
                            for library in p.libraries {
                                li { key: "{library.title}", class: "flex justify-between border-t border-ctp-surface0 py-1",
                                    span { "{library.title}" }
                                    span { class: "text-ctp-subtext0", "{library.items} items" }
                                }
                            }
                        }
                    },
                }
            }
            PathCheck {}
        }
    }
}

#[component]
fn CopyButton(text: String) -> Element {
    let mut copied = use_signal(|| false);
    rsx! {
        button {
            class: BUTTON,
            onclick: move |_| {
                if let Some(window) = web_sys::window() {
                    let _ = window.navigator().clipboard().write_text(&text);
                    copied.set(true);
                }
            },
            if copied() { "Copied" } else { "Copy" }
        }
    }
}

#[component]
fn PathCheck() -> Element {
    let mut result = use_signal(|| None::<Result<Vec<PathReport>, ApiError>>);
    let mut busy = use_signal(|| false);
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
    rsx! {
        section { class: CARD,
            div { class: "mb-3 flex items-center justify-between",
                h2 { class: "font-semibold", "Media paths" }
                button { class: BUTTON, disabled: busy(), onclick: run,
                    if busy() { "Checking…" } else { "Check every file" }
                }
            }
            p { class: "mb-2 text-sm text-ctp-subtext0",
                "Whether this server can read every file Plex lists, through the path mapping."
            }
            match result() {
                None => rsx! {},
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(reports)) => rsx! {
                    ul { class: "space-y-2 text-sm",
                        for r in reports {
                            li { key: "{r.library}",
                                div { class: "flex justify-between",
                                    span { "{r.library}" }
                                    if r.no_mapping.count == 0 && r.unreadable.count == 0 {
                                        span { class: "text-ctp-green", "all {r.ok} readable" }
                                    } else {
                                        span { class: "text-ctp-red",
                                            "{r.ok} readable, {r.no_mapping.count} with no mapping, {r.unreadable.count} missing"
                                        }
                                    }
                                }
                                for example in r.no_mapping.examples.iter().chain(&r.unreadable.examples) {
                                    div { class: "font-mono text-xs text-ctp-subtext0 break-all", "{example}" }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}
