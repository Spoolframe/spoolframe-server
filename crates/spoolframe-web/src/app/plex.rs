//! Plex links and watch sync in the pages (P3-12), as the design's
//! WebPlexAccountStates, WebUserSessions, WebLibraries and
//! WebLibrariesStates boards draw them: My profile's Plex account card,
//! the user page's Plex account section, and Server › Libraries' watch
//! sync panel and what each direction means.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, MeApiClient, PlexLinkState, PlexSyncOverview, PlexUser, error_code,
};

use super::admin::{Said, say, sleep};
use super::{Alert, icons, now, rpc};
use crate::logic::ApiError;
use crate::plex_sync::{
    DIRECTIONS, direction_meaning, direction_name, last_run_line, linked_as, panel_name,
    profile_line, schedule_line, time_left, user_page_line,
};

/// How often a page with sync runs on it asks again, so "last run" stays
/// true.
const REFRESH_MS: i32 = 30_000;

/// How often the card asks again while a run is going.
const SOON_MS: i32 = 2_000;

/// How often a PIN is checked while plex.tv has not answered.
const PIN_CHECK_MS: i32 = 2_000;

/// Where linking is, in My profile.
#[derive(Debug, Clone, PartialEq)]
enum Linking {
    /// Nothing going on: the card shows the link as it is.
    Idle,
    /// plex.tv is being asked for a PIN.
    Asking,
    /// The Plex window is open, plex.tv has not answered yet.
    Waiting {
        id: u64,
        auth_url: String,
        expires_at: Option<i64>,
    },
    /// plex.tv could not be reached.
    Unreachable,
    /// The PIN expired, or Spoolframe was not allowed.
    Expired,
    Failed(String),
}

async fn my_link() -> Result<PlexUser, ApiError> {
    let client = rpc::client().await?;
    client.plex_link().await.map_err(rpc::error)
}

/// The card's head: a teal link outline, as every card's icon is
/// (WebProfile), and its name. Plex's orange stays on the linked line.
#[component]
fn PlexHead() -> Element {
    rsx! {
        div { class: "card-head",
            icons::LinkChain { size: 18 }
            h2 { "Plex account" }
        }
    }
}

/// Open `url` in a new window (plex.tv's sign-in).
pub(super) fn open(url: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.open_with_url_and_target(url, "_blank").ok().flatten())
        .is_some()
}

/// Where Plex's sign-in window ends once Spoolframe is allowed. The page
/// that opened the window is the one that waits for plex.tv and links, so
/// this one only closes itself, which a window a script opened may do,
/// and says so if the browser keeps it open.
#[component]
pub fn PlexSignedIn() -> Element {
    use_hook(|| {
        if let Some(window) = web_sys::window() {
            let _ = window.close();
        }
    });
    rsx! {
        super::Centered {
            section { class: "glass-card",
                div { class: "flex flex-col items-center gap-3 text-center",
                    span { class: "flex text-ctp-green", icons::CircleCheck { size: 28 } }
                    h1 { class: "m-0 text-2xl font-bold", "Signed in with Plex" }
                    p { class: "m-0 text-sm text-ctp-subtext1", "You can close this window and go back to Spoolframe." }
                }
            }
        }
    }
}

/// WebPlexAccountStates: My profile's Plex account card.
#[component]
pub fn PlexAccountCard() -> Element {
    let mut me = use_resource(my_link);
    let mut linking = use_signal(|| Linking::Idle);
    let mut unlinking = use_signal(|| false);
    // Bumped while waiting, so the countdown moves.
    let mut tick = use_signal(|| 0u32);
    use_hook(move || {
        spawn(async move {
            let mut ticks = 0;
            loop {
                sleep(SOON_MS).await;
                ticks += 1;
                // Every couple of seconds while a run is going or the first
                // is due (a new link runs at once), else now and then.
                let soon = matches!(
                    &*me.peek(),
                    Some(Ok(u)) if u.link.is_some() && (u.running || u.last_run.is_none())
                );
                if *linking.peek() == Linking::Idle && (soon || ticks % (REFRESH_MS / SOON_MS) == 0)
                {
                    me.restart();
                }
            }
        });
    });
    let link = move |_| {
        // Opened now, from the click: a window opened after an answer from
        // the server is one a browser may block.
        let window =
            web_sys::window().and_then(|w| w.open_with_url_and_target("", "_blank").ok().flatten());
        linking.set(Linking::Asking);
        spawn(async move {
            let begun = async {
                let client = rpc::client().await?;
                client.plex_link_begin().await.map_err(rpc::error)
            };
            let pin = match begun.await {
                Ok(pin) => pin,
                Err(e) => {
                    if let Some(window) = &window {
                        let _ = window.close();
                    }
                    linking.set(failed(e));
                    return;
                }
            };
            if let Some(window) = &window {
                let _ = window.location().set_href(&pin.auth_url);
            }
            let id = pin.id;
            linking.set(Linking::Waiting {
                id,
                auth_url: pin.auth_url,
                expires_at: pin.expires_in.map(|s| now() + s as i64),
            });
            loop {
                sleep(PIN_CHECK_MS).await;
                tick += 1;
                if !matches!(&*linking.peek(), Linking::Waiting { id: waiting, .. } if *waiting == id)
                {
                    return;
                }
                let checked = async {
                    let client = rpc::client().await?;
                    client.plex_link_check(id).await.map_err(rpc::error)
                };
                match checked.await {
                    Ok(PlexLinkState::Waiting) => {}
                    Ok(PlexLinkState::Linked { .. }) => {
                        linking.set(Linking::Idle);
                        me.restart();
                        return;
                    }
                    Ok(PlexLinkState::Expired) => {
                        linking.set(Linking::Expired);
                        return;
                    }
                    Err(e) => {
                        linking.set(failed(e));
                        return;
                    }
                }
            }
        });
    };
    let _ = tick();
    let body = match linking() {
        Linking::Asking => rsx! {
            span { class: "flex items-center gap-2 text-[13px] font-semibold text-ctp-blue", role: "status",
                span { class: "flex animate-spin", icons::Loader { size: 15 } }
                "Asking plex.tv…"
            }
        },
        Linking::Waiting {
            auth_url,
            expires_at,
            ..
        } => rsx! {
            p { class: "m-0 text-sm", "A Plex window opened. Sign in there and allow Spoolframe." }
            span { class: "flex items-center gap-2 text-[13px] font-semibold text-ctp-blue", role: "status",
                span { class: "flex animate-spin", icons::Loader { size: 15 } }
                match expires_at {
                    Some(at) => rsx! { "Waiting for plex.tv · {time_left(now(), at)}" },
                    None => rsx! { "Waiting for plex.tv" },
                }
            }
            div { class: "flex flex-wrap gap-2.5",
                button { class: "btn", onclick: move |_| { open(&auth_url); }, icons::ExternalLink { size: 15 } "Open the Plex window again" }
                button { class: "btn-plain h-[30px] px-4 text-[13px]", onclick: move |_| linking.set(Linking::Idle), "Cancel" }
            }
            span { class: "text-xs text-ctp-subtext0", "If no window appeared, your browser may have blocked it." }
        },
        Linking::Unreachable => rsx! {
            div { class: "alert alert-error", role: "alert",
                icons::CircleX {}
                span { class: "flex flex-col gap-[3px]",
                    b { "Couldn't reach plex.tv" }
                    span { class: "text-ctp-text", "Linking needs the Internet for a moment. Check the server's connection and try again." }
                }
            }
            button { class: "btn-dialog btn-dialog-primary w-full", onclick: link, icons::RotateCcw {} "Try again" }
        },
        Linking::Expired => rsx! {
            div { class: "alert alert-warning", role: "alert",
                icons::Warning {}
                span { class: "flex flex-col gap-[3px]",
                    b { "Linking didn't finish" }
                    span { class: "text-ctp-text", "The code expired or Spoolframe wasn't allowed. Nothing was linked." }
                }
            }
            button { class: "btn-dialog btn-dialog-primary w-full", onclick: link, icons::ExternalLink {} "Link with Plex" }
        },
        Linking::Failed(why) => rsx! {
            Alert { text: why }
            button { class: "btn-dialog btn-dialog-primary w-full", onclick: link, icons::ExternalLink {} "Link with Plex" }
        },
        Linking::Idle => match me.read().clone() {
            None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
            Some(Err(e)) => rsx! { Alert { text: e.explain() } },
            Some(Ok(user)) => match user.link.clone() {
                None => rsx! {
                    p { class: "m-0 text-sm", "Link your Plex account to sync what you've watched." }
                    ul { class: "plex-points",
                        li { "Spoolframe keeps your watched state in step with your Plex account, for the libraries the admin turned sync on for." }
                        li { "The token plex.tv gives back reaches your whole Plex account. The server keeps it secret and uses it only for this." }
                        li { "Unlink any time; the token is deleted at once." }
                    }
                    button { class: "btn-dialog btn-dialog-primary w-full", onclick: link, icons::ExternalLink {} "Link with Plex" }
                },
                Some(l) => rsx! {
                    span { class: "flex items-center gap-2 text-[15px] font-bold",
                        span { class: "flex text-ctp-green", icons::CircleCheck { size: 18 } }
                        "{linked_as(&l)}"
                    }
                    span { class: "text-[13px] text-ctp-subtext0",
                        "{profile_line(now(), user.last_run.as_ref(), user.running)}"
                    }
                    for note in user.last_run.iter().flat_map(|r| r.notes.clone()) {
                        span { class: "text-xs text-ctp-red", "{note}" }
                    }
                    if l.by_config {
                        span { class: "text-xs text-ctp-subtext0",
                            "The server's config file links you, with each Plex server's own sign-in; only that file can unlink it. Link your own Plex account to list its servers when adding a library: it takes the config's place until you unlink it."
                        }
                        button { class: "btn w-full h-9", onclick: link, icons::ExternalLink { size: 15 } "Link with Plex instead" }
                    } else {
                        button { class: "btn btn-danger w-full h-9", onclick: move |_| unlinking.set(true), icons::Unlink { size: 15 } "Unlink" }
                    }
                },
            },
        },
    };
    rsx! {
        section { class: "card",
            PlexHead {}
            div { class: "card-body", {body} }
        }
        if unlinking() {
            UnlinkDialog {
                title: "Unlink your Plex account?",
                text: "The token is deleted and syncing stops. What you've watched stays as it is, here and in Plex.",
                username: None,
                on_done: move |done: Option<Result<(), String>>| {
                    unlinking.set(false);
                    match done {
                        Some(Err(why)) => linking.set(Linking::Failed(why)),
                        Some(Ok(())) => me.restart(),
                        None => {}
                    }
                },
            }
        }
    }
}

/// What a failed call to plex.tv means for the card.
fn failed(e: ApiError) -> Linking {
    if e.code == error_code::UNAVAILABLE {
        Linking::Unreachable
    } else {
        Linking::Failed(e.explain())
    }
}

/// WebPlexAccountStates, 4: unlinking, asked first. `username` is the user
/// an admin unlinks; `None` the caller's own.
#[component]
fn UnlinkDialog(
    title: String,
    text: String,
    username: Option<String>,
    on_done: EventHandler<Option<Result<(), String>>>,
) -> Element {
    let mut busy = use_signal(|| false);
    let go = move |_| {
        busy.set(true);
        let username = username.clone();
        spawn(async move {
            let unlinked = async {
                let client = rpc::client().await?;
                match username {
                    Some(username) => client.user_plex_unlink(username).await,
                    None => client.plex_unlink().await,
                }
                .map_err(rpc::error)
            };
            let done = unlinked.await.map_err(|e| e.explain());
            busy.set(false);
            on_done.call(Some(done));
        });
    };
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::Unlink { size: 22 }
                    h1 { "{title}" }
                }
                p { "{text}" }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: go, "Unlink" }
                }
            }
        }
    }
}

async fn overview() -> Result<PlexSyncOverview, ApiError> {
    let client = rpc::client().await?;
    client.plex_sync_overview().await.map_err(rpc::error)
}

/// Sync `username` now, or with `None` everyone linked; what to say.
async fn sync_now(username: Option<String>) -> (bool, String) {
    let synced = async {
        let client = rpc::client().await?;
        client
            .plex_watch_sync(username.clone(), false)
            .await
            .map_err(rpc::error)
    };
    match synced.await {
        Ok(reports) => match (username, reports.as_slice()) {
            (Some(_), [one]) => (
                one.notes.is_empty() && one.failed.is_empty(),
                format!(
                    "{} synced: {} pulled, {} pushed",
                    one.username,
                    one.pulled.len(),
                    one.pushed.len()
                ),
            ),
            (_, []) => (true, "Nobody has linked a Plex account yet".into()),
            (_, all) => (
                all.iter()
                    .all(|r| r.notes.is_empty() && r.failed.is_empty()),
                format!(
                    "Synced {} {}",
                    all.len(),
                    if all.len() == 1 { "person" } else { "people" }
                ),
            ),
        },
        Err(e) => (false, e.explain()),
    }
}

/// WebUserSessions: a user's Plex account, for an admin. Seen, synced and
/// unlinked, never linked: only the user can link one.
#[component]
pub fn UserPlex(username: String, name: String, toast: Signal<Option<Said>>) -> Element {
    let mut list = use_resource(overview);
    let mut unlinking = use_signal(|| false);
    let mut syncing = use_signal(|| false);
    let current = list.read().clone();
    let user = current
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .and_then(|o| o.users.iter().find(|u| u.username == username).cloned());
    let sync = {
        let username = username.clone();
        move |_| {
            let username = username.clone();
            syncing.set(true);
            spawn(async move {
                let (ok, text) = sync_now(Some(username)).await;
                say(toast, ok, text);
                syncing.set(false);
                list.restart();
            });
        }
    };
    rsx! {
        div { class: "section-head",
            h2 { "Plex account" }
            span { "Used to sync {name}'s watch state with Plex. Only {name} can link an account, in My profile." }
        }
        match (current, user) {
            (None, _) => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
            (Some(Err(e)), _) => rsx! { Alert { text: e.explain() } },
            (Some(Ok(_)), None) => rsx! {},
            (Some(Ok(_)), Some(user)) => rsx! {
                section { class: "card",
                    div { class: "card-body",
                        div { class: "row",
                            span { class: "flex min-w-0 items-center gap-3",
                                span { class: if user.link.is_some() { "flex text-[#e5a00d]" } else { "flex text-ctp-overlay0" }, icons::Link { size: 18 } }
                                span { class: "row-what",
                                    match &user.link {
                                        Some(l) => rsx! {
                                            b { "{linked_as(l)}" }
                                            span { class: "row-sub", "{user_page_line(now(), user.last_run.as_ref(), user.running || syncing())}" }
                                            for note in user.last_run.iter().flat_map(|r| r.notes.clone()) {
                                                span { class: "row-sub text-ctp-red", "{note}" }
                                            }
                                        },
                                        None => rsx! {
                                            b { "Not linked" }
                                            span { class: "row-sub", "Not linked to Plex · not synced" }
                                        },
                                    }
                                }
                            }
                            if let Some(l) = &user.link {
                                span { class: "row-do",
                                    button { class: "btn", disabled: syncing(), onclick: sync, icons::RotateCcw {} "Sync now" }
                                    if !l.by_config {
                                        button { class: "btn btn-danger", onclick: move |_| unlinking.set(true), icons::Unlink { size: 15 } "Unlink" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
        if unlinking() {
            UnlinkDialog {
                title: format!("Unlink {name}'s Plex account?"),
                text: format!("The token is deleted and {name}'s watch state stops syncing. What {name} has watched stays as it is, here and in Plex. Only {name} can link an account again."),
                username: Some(username.clone()),
                on_done: move |done: Option<Result<(), String>>| {
                    unlinking.set(false);
                    match done {
                        Some(Ok(())) => {
                            say(toast, true, format!("{name}'s Plex account unlinked"));
                            list.restart();
                        }
                        Some(Err(why)) => say(toast, false, why),
                        None => {}
                    }
                },
            }
        }
    }
}

/// WebLibraries: the watch sync panel, under the libraries.
#[component]
pub fn SyncPanel(toast: Signal<Option<Said>>) -> Element {
    let mut list = use_resource(overview);
    let mut syncing = use_signal(|| None::<Option<String>>);
    use_hook(move || {
        spawn(async move {
            loop {
                sleep(REFRESH_MS).await;
                list.restart();
            }
        });
    });
    let mut run = move |username: Option<String>| {
        syncing.set(Some(username.clone()));
        spawn(async move {
            let (ok, text) = sync_now(username).await;
            say(toast, ok, text);
            syncing.set(None);
            list.restart();
        });
    };
    let current = list.read().clone();
    rsx! {
        section { class: "card",
            div { class: "card-head flex-wrap",
                h2 { "Watch sync" }
                if let Some(Ok(o)) = &current {
                    span { class: "text-xs text-ctp-subtext0", "{schedule_line(o.interval_secs)}" }
                }
                button { class: "btn", disabled: syncing().is_some(), onclick: move |_| run(None), icons::RotateCcw {} "Sync everyone" }
            }
            div { class: "card-body gap-0 py-1",
                match current {
                    None => rsx! { p { class: "m-0 py-3 text-sm text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(o)) => rsx! {
                        for user in o.users {
                            div { key: "{user.username}", class: "sync-row",
                                span { class: "row-what",
                                    b { "{panel_name(&user)}" }
                                    span { class: "row-sub", "{last_run_line(now(), &user)}" }
                                    for note in user.last_run.iter().flat_map(|r| r.notes.clone()) {
                                        span { class: "row-sub text-ctp-red", "{note}" }
                                    }
                                }
                                for (n, label) in [
                                    (user.last_run.as_ref().filter(|_| user.link.is_some()).map(|r| r.pulled), "pulled"),
                                    (user.last_run.as_ref().filter(|_| user.link.is_some()).map(|r| r.pushed), "pushed"),
                                    (user.last_run.as_ref().filter(|_| user.link.is_some()).map(|r| r.skipped), "skipped"),
                                ] {
                                    span { class: "sync-count",
                                        b { {n.map_or("—".to_owned(), |n| crate::logic::thousands(u64::from(n)))} }
                                        span { "{label}" }
                                    }
                                }
                                span { class: "flex justify-end",
                                    if user.link.is_some() {
                                        button {
                                            class: "btn",
                                            disabled: syncing().is_some(),
                                            onclick: {
                                                let username = user.username.clone();
                                                move |_| run(Some(username.clone()))
                                            },
                                            icons::RotateCcw {}
                                            "Sync now"
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
}

/// WebLibrariesStates, 5: what each direction does.
#[component]
pub fn SyncExplained() -> Element {
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Watch sync" } }
            div { class: "card-body",
                dl { class: "sync-meanings",
                    for d in DIRECTIONS {
                        dt { "{direction_name(d)}" }
                        dd { "{direction_meaning(d)}" }
                    }
                }
                span { class: "text-xs text-ctp-subtext0", "Only for people who linked their Plex account in My profile." }
            }
        }
    }
}
