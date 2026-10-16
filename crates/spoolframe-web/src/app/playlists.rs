//! Playlists (PX-40): My playlists (a consumer's Playlists tab, an admin's
//! sidebar link), one playlist and its entries, the sharing dialog, the
//! Plex import, the admin's Libraries › Playlists for server-wide ones,
//! and "Add to playlist" and the Playlists card on an item's page.
//!
//! Liked and All (PX-43) are listed first among the user's own: the
//! server sends them so, and says they cannot be managed, so they have no
//! Edit, Share or Delete; All, which nobody edits, no move or remove
//! either. A heart likes a track or an album (`me.like`, `me.unlike`).
//!
//! Every call is a `me.playlist*` one: the server decides who may do
//! what, and the pages only leave out what the answer says the user
//! cannot do. The web UI plays nothing: playing a playlist is the apps'.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    CatalogKind, Item, MeApiClient, NewPlaylist, PlaylistBuiltin, PlaylistChanges, PlaylistDetail,
    PlaylistEditOp, PlaylistInfo, PlaylistKinds, PlaylistOwner, PlaylistSharing,
    PlexPlaylistImport, ShareableUser, UserInfo, error_code,
};

use super::admin::{Said, Toast, say};
use super::{Alert, Field, Route, icons, rpc};
use crate::catalog::{item_context, item_kind, row_title};
use crate::logic::ApiError;
use crate::playlists::{
    Access, Heart, HeldIn, add_targets, builtin_line, count_line, empty_text, kinds_line,
    move_target, no_target_text, sections, share_rows, sharing_line, sharing_of, widened,
};

/// How many entries one page of a playlist shows.
const PAGE: u32 = 200;

async fn list() -> Result<Vec<PlaylistInfo>, ApiError> {
    let client = rpc::client().await?;
    client.playlists().await.map_err(rpc::error)
}

async fn candidates() -> Result<Vec<ShareableUser>, ApiError> {
    let client = rpc::client().await?;
    client.playlist_share_candidates().await.map_err(rpc::error)
}

async fn detail(id: String, offset: u32) -> Result<PlaylistDetail, ApiError> {
    let client = rpc::client().await?;
    client.playlist(id, offset, PAGE).await.map_err(rpc::error)
}

async fn edit(id: String, revision: u64, edits: Vec<PlaylistEditOp>) -> Result<u64, ApiError> {
    let client = rpc::client().await?;
    client
        .playlist_edit(id, revision, edits)
        .await
        .map_err(rpc::error)
}

/// Like `items` (`like`), or unlike them.
async fn like_items(items: Vec<String>, like: bool) -> Result<PlaylistInfo, ApiError> {
    let client = rpc::client().await?;
    let liked = if like {
        client.like(items).await
    } else {
        client.unlike(items).await
    };
    liked.map_err(rpc::error)
}

fn is_admin() -> bool {
    use_context::<UserInfo>().role.is_admin()
}

/// A playlist's icon: a heart for Liked, else what it holds.
#[component]
fn PlaylistIcon(kinds: PlaylistKinds, builtin: Option<PlaylistBuiltin>) -> Element {
    match builtin {
        Some(PlaylistBuiltin::Liked) => rsx! { icons::Heart { size: 18, full: true } },
        _ => rsx! { KindsIcon { kinds } },
    }
}

/// A track's or an album's heart, as a button: a click likes it (an
/// album's tracks not liked yet), or, full, unlikes it; `on_changed`
/// then, to read the page again. With `word`, "Like" or "Liked" beside
/// it, as on an item's page; without, the heart alone, as in a list.
#[component]
pub fn HeartButton(
    id: String,
    heart: Heart,
    kind: CatalogKind,
    title: String,
    #[props(default)] word: bool,
    on_changed: EventHandler<()>,
) -> Element {
    let mut busy = use_signal(|| false);
    let toast = use_signal(|| None::<Said>);
    let action = heart.action(kind);
    let click = move |_| {
        busy.set(true);
        let id = id.clone();
        spawn(async move {
            match like_items(vec![id], heart.likes()).await {
                Ok(_) => on_changed.call(()),
                Err(e) => say(toast, false, e.explain()),
            }
            busy.set(false);
        });
    };
    let shown = rsx! {
        span { class: if heart == Heart::Empty { "text-ctp-subtext0" } else { "text-ctp-red" },
            icons::Heart { size: if word { 14 } else { 16 }, full: heart == Heart::Full, half: heart == Heart::Half }
        }
    };
    rsx! {
        if word {
            button { class: "btn shrink-0", aria_label: "{action}: {title}", title: "{action}", aria_pressed: "{heart != Heart::Empty}", disabled: busy(), onclick: click,
                {shown}
                "{heart.word()}"
            }
        } else {
            button { class: "icon-button", aria_label: "{action}: {title}", title: "{action}", aria_pressed: "{heart != Heart::Empty}", disabled: busy(), onclick: click,
                {shown}
            }
        }
        Toast { said: toast }
    }
}

/// The icon of what a playlist holds: music, else films and episodes.
#[component]
fn KindsIcon(kinds: PlaylistKinds) -> Element {
    if kinds.tracks && !kinds.movies && !kinds.episodes {
        rsx! { icons::Music { size: 18 } }
    } else {
        rsx! { icons::Film { size: 18 } }
    }
}

/// My playlists: the user's own, those shared with them, and the
/// server-wide ones, with New playlist and the Plex import.
#[component]
pub fn MyPlaylists() -> Element {
    let admin = is_admin();
    let mut playlists = use_resource(list);
    let people = use_resource(candidates);
    let toast = use_signal(|| None::<Said>);
    let mut making = use_signal(|| false);
    let mut importing = use_signal(|| false);
    let nav = use_navigator();
    let current = playlists.read().clone();
    let names = people
        .read()
        .clone()
        .and_then(Result::ok)
        .unwrap_or_default();
    rsx! {
        div { class: "flex flex-col gap-6",
            div { class: "page-head",
                h1 { class: "page-title", "Playlists" }
                span { class: "flex flex-wrap gap-2",
                    button { class: "btn", onclick: move |_| importing.set(true),
                        icons::Download {}
                        "Import from Plex"
                    }
                    button { class: "btn-add", onclick: move |_| making.set(true),
                        icons::Plus { size: 16 }
                        "New playlist"
                    }
                }
            }
            p { class: "lede m-0 max-w-[680px]",
                "Films, episodes and music in an order of your own. Share one with people on this server, to watch or to edit. Play them from the Spoolframe apps."
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(all)) => {
                    let [mine, shared, everyone] = sections(&all);
                    rsx! {
                        Section { title: "Yours", playlists: mine.clone(), people: names.clone(),
                            empty: "None yet. Make one, or import yours from Plex."
                        }
                        if !shared.is_empty() {
                            Section { title: "Shared with you", playlists: shared, people: names.clone(), empty: "" }
                        }
                        if !everyone.is_empty() || admin {
                            Section { title: "On this server", playlists: everyone, people: names.clone(),
                                empty: "None yet.",
                                manage: admin,
                            }
                        }
                    }
                }
            }
        }
        if making() {
            NewPlaylistDialog {
                server_wide: false,
                on_done: move |made: Option<PlaylistInfo>| {
                    making.set(false);
                    if let Some(made) = made {
                        nav.push(Route::PlaylistView { id: made.id });
                    }
                },
            }
        }
        if importing() {
            PlexImportDialog {
                on_done: move |said: Option<String>| {
                    importing.set(false);
                    if let Some(text) = said {
                        say(toast, true, text);
                        playlists.restart();
                    }
                },
            }
        }
        Toast { said: toast }
    }
}

/// One list of playlists, under a heading.
#[component]
fn Section(
    title: String,
    playlists: Vec<PlaylistInfo>,
    people: Vec<ShareableUser>,
    empty: String,
    /// An admin's: a link to where server-wide ones are managed.
    #[props(default)]
    manage: bool,
) -> Element {
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "{title}" }
                if manage {
                    Link { class: "text-sm", to: Route::ServerPlaylists {}, "Manage in Libraries › Playlists" }
                }
            }
            div { class: "card-body gap-0 py-0",
                if playlists.is_empty() {
                    p { class: "py-4 text-sm text-ctp-subtext0", "{empty}" }
                }
                for p in playlists {
                    PlaylistRow { key: "{p.id}", playlist: p.clone(), people: people.clone() }
                }
            }
        }
    }
}

#[component]
fn PlaylistRow(playlist: PlaylistInfo, people: Vec<ShareableUser>) -> Element {
    let p = playlist;
    let rights = if p.can_manage {
        ""
    } else if p.can_edit {
        " · You can edit"
    } else {
        " · View only"
    };
    let line = match p.builtin {
        Some(builtin) => builtin_line(builtin).to_owned(),
        None => format!(
            "{} · {}{rights}",
            kinds_line(p.kinds),
            sharing_line(&p.sharing, &people)
        ),
    };
    rsx! {
        Link { class: "cat-child", to: Route::PlaylistView { id: p.id.clone() },
            span { class: "text-ctp-teal", PlaylistIcon { kinds: p.kinds, builtin: p.builtin } }
            span { class: "flex min-w-0 flex-grow flex-col",
                b { class: "truncate text-sm", "{p.name}" }
                span { class: "text-xs text-ctp-subtext0", "{line}" }
            }
            span { class: "text-ctp-subtext0", icons::ChevronRight { size: 16 } }
        }
    }
}

/// The tabs over Libraries and its Playlists (the admin's).
#[component]
pub fn LibrariesTabs(playlists: bool) -> Element {
    rsx! {
        nav { class: "cat-tabs", aria_label: "Libraries",
            Link { class: if playlists { "cat-tab" } else { "cat-tab cat-tab-here" }, to: Route::Libraries {}, "Libraries" }
            Link { class: if playlists { "cat-tab cat-tab-here" } else { "cat-tab" }, to: Route::ServerPlaylists {}, "Playlists" }
        }
    }
}

/// Libraries › Playlists: the server-wide playlists, which every user
/// sees and admins edit.
#[component]
pub fn ServerPlaylists() -> Element {
    let playlists = use_resource(list);
    let mut making = use_signal(|| false);
    let nav = use_navigator();
    let current = playlists.read().clone();
    rsx! {
        div { class: "flex flex-col gap-6",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin", "Libraries" }
                button { class: "btn-add", onclick: move |_| making.set(true),
                    icons::Plus { size: 16 }
                    "New server-wide playlist"
                }
            }
            LibrariesTabs { playlists: true }
            p { class: "lede m-0 max-w-[760px]",
                "Playlists every user of this server sees, guests too. Any admin can change them; everyone else plays them. Your own playlists are under My playlists."
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(all)) => {
                    let [_, _, everyone] = sections(&all);
                    rsx! {
                        Section { title: "Server-wide playlists", playlists: everyone, people: Vec::new(), empty: "None yet." }
                    }
                }
            }
        }
        if making() {
            NewPlaylistDialog {
                server_wide: true,
                on_done: move |made: Option<PlaylistInfo>| {
                    making.set(false);
                    if let Some(made) = made {
                        nav.push(Route::PlaylistView { id: made.id });
                    }
                },
            }
        }
    }
}

/// The kinds' three boxes; those in `fixed` stay ticked (a playlist's
/// kinds only widen).
#[component]
fn KindsBoxes(kinds: Signal<PlaylistKinds>, fixed: PlaylistKinds) -> Element {
    let mut kinds = kinds;
    let now = kinds();
    rsx! {
        fieldset { class: "flex flex-col gap-2 border-0 p-0 m-0",
            legend { class: "mb-1 text-sm font-semibold", "It holds" }
            label { class: "flex items-center gap-2 text-sm",
                input { r#type: "checkbox", checked: now.movies, disabled: fixed.movies,
                    onchange: move |e| kinds.write().movies = e.checked() }
                "Movies"
            }
            label { class: "flex items-center gap-2 text-sm",
                input { r#type: "checkbox", checked: now.episodes, disabled: fixed.episodes,
                    onchange: move |e| kinds.write().episodes = e.checked() }
                "Episodes"
            }
            label { class: "flex items-center gap-2 text-sm",
                input { r#type: "checkbox", checked: now.tracks, disabled: fixed.tracks,
                    onchange: move |e| kinds.write().tracks = e.checked() }
                "Music"
            }
        }
    }
}

#[component]
fn NewPlaylistDialog(server_wide: bool, on_done: EventHandler<Option<PlaylistInfo>>) -> Element {
    let name = use_signal(String::new);
    let kinds = use_signal(PlaylistKinds::default);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let empty = kinds() == PlaylistKinds::default();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        spawn(async move {
            let made = async {
                let client = rpc::client().await?;
                client
                    .playlist_create(NewPlaylist {
                        name: name().trim().to_owned(),
                        kinds: kinds(),
                        sharing: if server_wide {
                            PlaylistSharing::ServerWide
                        } else {
                            PlaylistSharing::Personal
                        },
                    })
                    .await
                    .map_err(rpc::error)
            };
            match made.await {
                Ok(made) => on_done.call(Some(made)),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    rsx! {
        div { class: "scrim",
            form { class: "dialog w-[440px]", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Plus { size: 22 }
                    h1 { if server_wide { "New server-wide playlist" } else { "New playlist" } }
                }
                Field { label: "Name", value: name }
                KindsBoxes { kinds, fixed: PlaylistKinds::default() }
                span { class: "hint",
                    "Only items of these kinds can be added. You can allow more later, never fewer."
                    if server_wide { " Every user of this server will see it." }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy() || empty || name().trim().is_empty(), "Create" }
                }
            }
        }
    }
}

/// One playlist: its entries, and what the user may do with it.
#[component]
pub fn PlaylistView(id: String) -> Element {
    let mut offset = use_signal(|| 0u32);
    let mut page = use_resource(use_reactive((&id,), move |(id,)| detail(id, offset())));
    let people = use_resource(candidates);
    let toast = use_signal(|| None::<Said>);
    let mut editing = use_signal(|| false);
    let mut sharing = use_signal(|| false);
    let mut deleting = use_signal(|| false);
    let admin = is_admin();
    let current = page.read().clone();
    let names = people
        .read()
        .clone()
        .and_then(Result::ok)
        .unwrap_or_default();
    let detail = match current {
        None => return rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
        Some(Err(e)) => {
            return rsx! {
                div { class: "flex flex-col gap-4",
                    Link { class: "text-sm", to: Route::MyPlaylists {}, "‹ Playlists" }
                    Alert { text: e.explain() }
                }
            };
        }
        Some(Ok(detail)) => detail,
    };
    let p = detail.playlist.clone();
    let back_to = back_to_route(p.owner, admin);
    let back_label = match back_to {
        Route::ServerPlaylists {} => "Libraries › Playlists",
        _ => "Playlists",
    };
    // An edit made, then the page read again; a conflict reloads too.
    let mut edited = move |result: Result<u64, ApiError>, done: String| {
        match result {
            Ok(_) => {
                if !done.is_empty() {
                    say(toast, true, done);
                }
            }
            Err(e) => say(toast, false, e.explain()),
        }
        page.restart();
    };
    let more_after = detail.offset + (detail.entries.len() as u32) < detail.total;
    rsx! {
        div { class: "flex flex-col gap-5",
            Link { class: "text-sm", to: back_to.clone(), "‹ {back_label}" }
            div { class: "page-head",
                span { class: "flex min-w-0 items-center gap-3",
                    span { class: "text-ctp-teal", PlaylistIcon { kinds: p.kinds, builtin: p.builtin } }
                    span { class: "flex min-w-0 flex-col gap-1",
                        h1 { class: "page-title m-0", "{p.name}" }
                        span { class: "text-sm text-ctp-subtext0",
                            match p.builtin {
                                Some(builtin) => format!("{} · {}", count_line(p.kinds, detail.total, detail.playable), builtin_line(builtin)),
                                None => format!("{} · {} · {}", count_line(p.kinds, detail.total, detail.playable), kinds_line(p.kinds), sharing_line(&p.sharing, &names)),
                            }
                        }
                    }
                }
                if p.can_manage {
                    span { class: "flex flex-wrap gap-2",
                        button { class: "btn", onclick: move |_| editing.set(true), icons::Pencil {} "Edit" }
                        if p.owner != PlaylistOwner::ServerWide {
                            button { class: "btn", onclick: move |_| sharing.set(true), icons::Share { size: 15 } "Share" }
                        }
                        button { class: "btn btn-danger", onclick: move |_| deleting.set(true), icons::Trash {} "Delete" }
                    }
                }
            }
            if !p.can_edit {
                div { class: "alert alert-info", role: "note",
                    icons::Eye {}
                    if p.builtin == Some(PlaylistBuiltin::All) {
                        span { "All is every track you can play, kept up to date by the server. Play it from the apps; shuffle plays the whole library." }
                    } else {
                        span { "You can play this playlist in the apps, not change it." }
                    }
                }
            }
            section { class: "card",
                div { class: "card-body gap-0 py-0",
                    if detail.entries.is_empty() {
                        p { class: "py-4 text-sm text-ctp-subtext0", "{empty_text(p.builtin, admin)}" }
                    }
                    for (i, entry) in detail.entries.iter().cloned().enumerate() {
                        {
                            let n = detail.offset as usize + i + 1;
                            let up = move_target(&detail.entries, i, true, more_after);
                            let down = move_target(&detail.entries, i, false, more_after);
                            let (pid, revision) = (p.id.clone(), p.revision);
                            let title = match &entry.item {
                                Item::Episode(e) => e.title.clone(),
                                other => row_title(other),
                            };
                            let music = matches!(item_kind(&entry.item), CatalogKind::Track);
                            rsx! {
                                div { key: "{entry.entry}", class: "cat-child",
                                    span { class: "w-8 shrink-0 text-right font-mono text-xs text-ctp-subtext0", "{n}" }
                                    span { class: "text-ctp-subtext0",
                                        if music { icons::Music { size: 16 } } else { icons::Film { size: 16 } }
                                    }
                                    span { class: "flex min-w-0 flex-grow flex-col",
                                        b { class: "truncate text-sm", "{title}" }
                                        span { class: "truncate text-xs text-ctp-subtext0", "{item_context(&entry.item)}" }
                                    }
                                    if entry.unavailable {
                                        span { class: "state state-warn cat-child-state", title: "Its file can't be read now; it's skipped when playing.",
                                            icons::Warning { size: 14 }
                                            "Unavailable"
                                        }
                                    }
                                    if music {
                                        HeartButton {
                                            id: entry.item.id().to_string(),
                                            heart: Heart::of_track(entry.liked),
                                            kind: CatalogKind::Track,
                                            title: title.clone(),
                                            on_changed: move |_| page.restart(),
                                        }
                                    }
                                    if p.can_edit {
                                        span { class: "flex shrink-0 items-center gap-1",
                                            button {
                                                class: "icon-button",
                                                aria_label: "Move {title} up",
                                                disabled: up.is_none(),
                                                onclick: {
                                                    let pid = pid.clone();
                                                    move |_| {
                                                        if let Some(before) = up {
                                                            let pid = pid.clone();
                                                            spawn(async move {
                                                                edited(edit(pid, revision, vec![PlaylistEditOp::Move { entry: entry.entry, before }]).await, String::new());
                                                            });
                                                        }
                                                    }
                                                },
                                                icons::ChevronUp {}
                                            }
                                            button {
                                                class: "icon-button",
                                                aria_label: "Move {title} down",
                                                disabled: down.is_none(),
                                                onclick: {
                                                    let pid = pid.clone();
                                                    move |_| {
                                                        if let Some(before) = down {
                                                            let pid = pid.clone();
                                                            spawn(async move {
                                                                edited(edit(pid, revision, vec![PlaylistEditOp::Move { entry: entry.entry, before }]).await, String::new());
                                                            });
                                                        }
                                                    }
                                                },
                                                icons::ChevronDown {}
                                            }
                                            button {
                                                class: "icon-button",
                                                aria_label: "Remove {title}",
                                                title: "Remove",
                                                onclick: {
                                                    let (pid, title) = (pid.clone(), title.clone());
                                                    move |_| {
                                                        let (pid, title) = (pid.clone(), title.clone());
                                                        spawn(async move {
                                                            edited(edit(pid, revision, vec![PlaylistEditOp::Remove { entry: entry.entry }]).await, format!("Removed {title}"));
                                                        });
                                                    }
                                                },
                                                icons::X {}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if detail.total > PAGE {
                div { class: "flex items-center gap-2",
                    button { class: "btn", disabled: offset() == 0, onclick: move |_| { offset.set(offset().saturating_sub(PAGE)); page.restart(); }, icons::ChevronLeft {} "Previous" }
                    button { class: "btn", disabled: !more_after, onclick: move |_| { offset.set(offset() + PAGE); page.restart(); }, "Next" icons::ChevronRight {} }
                    span { class: "text-[13px] text-ctp-subtext0", "{detail.offset + 1}–{detail.offset + detail.entries.len() as u32} of {detail.total}" }
                }
            }
        }
        if editing() {
            EditDialog {
                playlist: p.clone(),
                on_done: move |said: Option<String>| {
                    editing.set(false);
                    if let Some(text) = said {
                        say(toast, true, text);
                        page.restart();
                    }
                },
            }
        }
        if sharing() {
            ShareDialog {
                playlist: p.clone(),
                on_done: move |said: Option<String>| {
                    sharing.set(false);
                    if let Some(text) = said {
                        say(toast, true, text);
                        page.restart();
                    }
                },
            }
        }
        if deleting() {
            DeleteDialog { playlist: p.clone(), back: back_to, on_cancel: move |_| deleting.set(false) }
        }
        Toast { said: toast }
    }
}

/// Where a playlist's page goes back to, and goes after it is deleted.
fn back_to_route(owner: PlaylistOwner, admin: bool) -> Route {
    if owner == PlaylistOwner::ServerWide && admin {
        Route::ServerPlaylists {}
    } else {
        Route::MyPlaylists {}
    }
}

/// Rename it, or let it hold more kinds.
#[component]
fn EditDialog(playlist: PlaylistInfo, on_done: EventHandler<Option<String>>) -> Element {
    let name = use_signal(|| playlist.name.clone());
    let kinds = use_signal(|| playlist.kinds);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let id = playlist.id.clone();
    let was = playlist.kinds;
    let old_name = playlist.name.clone();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        let id = id.clone();
        let old_name = old_name.clone();
        spawn(async move {
            let new_name = name().trim().to_owned();
            let new_kinds = widened(was, kinds());
            let changes = PlaylistChanges {
                name: (new_name != old_name).then(|| new_name.clone()),
                kinds: (new_kinds != was).then_some(new_kinds),
                sharing: None,
            };
            if changes == PlaylistChanges::default() {
                on_done.call(None);
                return;
            }
            let saved = async {
                let client = rpc::client().await?;
                client
                    .playlist_update(id, changes)
                    .await
                    .map_err(rpc::error)
            };
            match saved.await {
                Ok(_) => on_done.call(Some("Saved".into())),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    rsx! {
        div { class: "scrim",
            form { class: "dialog w-[440px]", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Pencil { size: 22 }
                    h1 { "Edit playlist" }
                }
                Field { label: "Name", value: name }
                KindsBoxes { kinds, fixed: was }
                span { class: "hint", "A playlist can be allowed more kinds of item, never fewer." }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy() || name().trim().is_empty(), "Save" }
                }
            }
        }
    }
}

/// The sharing dialog: each user here, not shared, viewing or editing.
#[component]
fn ShareDialog(playlist: PlaylistInfo, on_done: EventHandler<Option<String>>) -> Element {
    let people = use_resource(candidates);
    let mut rows = use_signal(Vec::<(ShareableUser, Access)>::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let sharing_now = playlist.sharing.clone();
    use_effect(move || {
        if let Some(Ok(people)) = &*people.read() {
            rows.set(share_rows(&sharing_now, people));
        }
    });
    let id = playlist.id.clone();
    let save = move |_| {
        busy.set(true);
        let id = id.clone();
        spawn(async move {
            let sharing = sharing_of(&rows.peek());
            let saved = async {
                let client = rpc::client().await?;
                client
                    .playlist_update(
                        id,
                        PlaylistChanges {
                            sharing: Some(sharing),
                            ..PlaylistChanges::default()
                        },
                    )
                    .await
                    .map_err(rpc::error)
            };
            match saved.await {
                Ok(_) => on_done.call(Some("Sharing saved".into())),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let loaded = people.read().clone();
    rsx! {
        div { class: "scrim",
            section { class: "dialog dialog-wide", role: "dialog",
                div { class: "dialog-head",
                    icons::Share { size: 22 }
                    h1 { "Share “{playlist.name}”" }
                }
                p { "People who can view it see it in their apps and play it. People who can edit it also add, remove and reorder what's in it; only you rename, share or delete it." }
                match loaded {
                    None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(_)) if rows.read().is_empty() => rsx! {
                        p { class: "text-sm text-ctp-subtext0", "There's nobody else on this server to share it with yet." }
                    },
                    Some(Ok(_)) => rsx! {
                        div { class: "flex max-h-[320px] flex-col gap-2 overflow-y-auto",
                            for (i, (user, access)) in rows.read().iter().cloned().enumerate() {
                                div { key: "{user.user_id}", class: "flex items-center justify-between gap-3",
                                    span { class: "truncate text-sm", "{user.display_name}" }
                                    select {
                                        class: "sync-select",
                                        aria_label: "Access for {user.display_name}",
                                        value: "{access.value()}",
                                        onchange: move |e| {
                                            if let Some(row) = rows.write().get_mut(i) {
                                                row.1 = Access::parse(&e.value());
                                            }
                                        },
                                        for a in Access::ALL {
                                            option { value: "{a.value()}", selected: a == access, "{a.label()}" }
                                        }
                                    }
                                }
                            }
                        }
                    },
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", disabled: busy() || !matches!(*people.read(), Some(Ok(_))), onclick: save, "Save" }
                }
            }
        }
    }
}

#[component]
fn DeleteDialog(playlist: PlaylistInfo, back: Route, on_cancel: EventHandler<()>) -> Element {
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let nav = use_navigator();
    let id = playlist.id.clone();
    let go = move |_| {
        busy.set(true);
        let (id, back) = (id.clone(), back.clone());
        spawn(async move {
            let deleted = async {
                let client = rpc::client().await?;
                client.playlist_delete(id).await.map_err(rpc::error)
            };
            match deleted.await {
                Ok(()) => {
                    nav.replace(back);
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let who = match playlist.owner {
        PlaylistOwner::ServerWide => "everyone on this server",
        _ => "you and everyone it's shared with",
    };
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::Trash { size: 22 }
                    h1 { "Delete “{playlist.name}”?" }
                }
                p { "It's gone for {who}, and this cannot be undone. What's in it stays in the library." }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_cancel.call(()), "Cancel" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: go, "Delete" }
                }
            }
        }
    }
}

/// The Plex import: a dry run first, saying what would be made and what
/// didn't map; then, if the user goes on, the import itself.
#[component]
fn PlexImportDialog(on_done: EventHandler<Option<String>>) -> Element {
    let dry = use_resource(|| async {
        let client = rpc::client().await?;
        client.playlist_plex_import(true).await.map_err(rpc::error)
    });
    let mut done = use_signal(|| None::<PlexPlaylistImport>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let import = move |_| {
        busy.set(true);
        spawn(async move {
            let made = async {
                let client = rpc::client().await?;
                client.playlist_plex_import(false).await.map_err(rpc::error)
            };
            match made.await {
                Ok(import) => done.set(Some(import)),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let shown: Option<Result<(PlexPlaylistImport, bool), ApiError>> = match done() {
        Some(import) => Some(Ok((import, false))),
        None => dry.read().clone().map(|r| r.map(|i| (i, true))),
    };
    let can_import = matches!(&shown, Some(Ok((i, true))) if !i.made.is_empty());
    let finished = done().map(|i| crate::playlists::import_line(&i, false));
    rsx! {
        div { class: "scrim",
            section { class: "dialog dialog-wide", role: "dialog",
                div { class: "dialog-head",
                    icons::Download { size: 22 }
                    h1 { "Import from Plex" }
                }
                p { "Your Plex playlists become playlists of yours here, once. Nothing is kept in sync afterwards." }
                match shown {
                    None => rsx! { p { class: "text-ctp-subtext0", "Reading your Plex playlists…" } },
                    Some(Err(e)) if e.code == error_code::NOT_CONFIGURED => rsx! {
                        Alert { text: format!("{} Link your Plex account under My profile first.", e.explain()) }
                    },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok((import, dry_run))) => rsx! { ImportReport { import, dry_run } },
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    if let Some(line) = finished {
                        button { class: "btn-dialog btn-dialog-primary", onclick: move |_| on_done.call(Some(line.clone())), "Done" }
                    } else {
                        button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Cancel" }
                        button { class: "btn-dialog btn-dialog-primary", disabled: busy() || !can_import, onclick: import, "Import" }
                    }
                }
            }
        }
    }
}

#[component]
fn ImportReport(import: PlexPlaylistImport, dry_run: bool) -> Element {
    rsx! {
        p { class: "m-0 text-sm font-semibold", "{crate::playlists::import_line(&import, dry_run)}" }
        div { class: "flex max-h-[320px] flex-col gap-3 overflow-y-auto",
            if !import.made.is_empty() {
                div { class: "flex flex-col gap-1",
                    span { class: "row-label", if dry_run { "Would be made" } else { "Made" } }
                    for m in import.made.iter() {
                        span { key: "{m.name}", class: "text-sm",
                            "{m.name} "
                            span { class: "text-ctp-subtext0", "· {crate::logic::count(u64::from(m.entries), \"item\")}" }
                        }
                    }
                }
            }
            if !import.unmapped.is_empty() {
                div { class: "flex flex-col gap-1",
                    span { class: "row-label", "Didn't map" }
                    for (i, u) in import.unmapped.iter().enumerate() {
                        span { key: "{i}", class: "text-sm",
                            match &u.title {
                                Some(title) => rsx! { "{u.playlist} › {title} " },
                                None => rsx! { "{u.playlist} " },
                            }
                            span { class: "text-ctp-subtext0", "· {u.reason}" }
                        }
                    }
                }
            }
            for (i, note) in import.notes.iter().enumerate() {
                span { key: "note{i}", class: "hint", "{note}" }
            }
        }
    }
}

/// "Add to playlist" on an item's page: the playlists the user can change
/// whose kinds take it.
#[component]
pub fn AddToPlaylist(item: Item) -> Element {
    let mut open = use_signal(|| false);
    let toast = use_signal(|| None::<Said>);
    rsx! {
        button { class: "btn shrink-0", onclick: move |_| open.set(true),
            icons::Plus { size: 14 }
            "Add to playlist"
        }
        if open() {
            AddDialog { item: item.clone(), toast, on_close: move |_| open.set(false) }
        }
        Toast { said: toast }
    }
}

#[component]
fn AddDialog(item: Item, toast: Signal<Option<Said>>, on_close: EventHandler<()>) -> Element {
    let playlists = use_resource(list);
    let mut busy = use_signal(|| false);
    let kind = item_kind(&item);
    let id = item.id().to_string();
    let current = playlists.read().clone();
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: "dialog-head",
                    icons::Plus { size: 22 }
                    h1 { "Add to playlist" }
                }
                match current {
                    None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(all)) => {
                        let targets = add_targets(&all, kind);
                        if targets.is_empty() {
                            rsx! { p { class: "text-sm text-ctp-subtext0", "{no_target_text(kind)}" } }
                        } else {
                            rsx! {
                                div { class: "flex max-h-[320px] flex-col gap-1 overflow-y-auto",
                                    for p in targets {
                                        button {
                                            key: "{p.id}",
                                            class: "btn justify-start",
                                            disabled: busy(),
                                            onclick: {
                                                let (pid, name, item_id, revision) = (p.id.clone(), p.name.clone(), id.clone(), p.revision);
                                                move |_| {
                                                    busy.set(true);
                                                    let (pid, name, item_id) = (pid.clone(), name.clone(), item_id.clone());
                                                    spawn(async move {
                                                        let added = edit(pid, revision, vec![PlaylistEditOp::Add { id: item_id, before: None }]).await;
                                                        match added {
                                                            Ok(_) => say(toast, true, format!("Added to {name}")),
                                                            Err(e) => say(toast, false, e.explain()),
                                                        }
                                                        busy.set(false);
                                                        on_close.call(());
                                                    });
                                                }
                                            },
                                            KindsIcon { kinds: p.kinds }
                                            "{p.name}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_close.call(()), "Cancel" }
                }
            }
        }
    }
}

/// An item page's Playlists card: the playlists the user sees that hold
/// it (or, for a show, season, album or artist, any of its episodes or
/// tracks).
#[component]
pub fn HeldInCard(held: Vec<HeldIn>) -> Element {
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Playlists" } }
            div { class: "card-body",
                if held.is_empty() {
                    p { class: "m-0 text-sm text-ctp-subtext0", "No playlist you see holds it." }
                }
                for p in held {
                    Link { key: "{p.id}", to: Route::PlaylistView { id: p.id.clone() },
                        if p.builtin.as_deref() == Some("liked") {
                            span { class: "text-ctp-red", icons::Heart { size: 13, full: true } }
                            " "
                        }
                        "{p.name}"
                        if p.owner_kind == "server_wide" {
                            span { class: "cat-quiet text-xs", " · server-wide" }
                        }
                    }
                }
            }
        }
    }
}
