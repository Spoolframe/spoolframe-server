//! Server › Libraries and its add-library flow (P3-11), as the design's
//! WebLibraries, WebLibraryAdd and WebLibrariesStates boards draw them,
//! with each library's watch sync and the sync panel (P3-12).

use std::cell::RefCell;

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, LibrariesOverview, LibraryEntry, LibraryKind, LibraryPathCheck, MeApiClient,
    PathMappingEntry, PlexServerEntry, PlexStatus, SyncDirection,
};

use super::admin::{Said, Toast, say, sleep};
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::wizard::{Account, AccountPanel, sign_in_with_plex};
use super::{Alert, Field, Route, icons, rpc};
use crate::libraries::{
    DropLine, FILES_HERE, PathCell, REMOTE_NOTE, add_label, added_text, down, down_text, drop_line,
    dropped_at, items_line, local_path, path_cell, refusals, remote, sample_line, section_line,
    source_line, with_path,
};
use crate::logic::{ApiError, host_port};
use crate::plex_sync::{
    DIRECTIONS, direction_changed, direction_name, direction_of, direction_value,
};

thread_local! {
    /// What a page that went back to Libraries had to say: shown there
    /// as a toast when it opens.
    static ARRIVING: RefCell<Option<(bool, String)>> = const { RefCell::new(None) };
}

/// Say `text` on the Libraries page that opens next.
fn say_next(ok: bool, text: String) {
    ARRIVING.with(|a| *a.borrow_mut() = Some((ok, text)));
}

async fn overview() -> Result<LibrariesOverview, ApiError> {
    let client = rpc::client().await?;
    client.libraries().await.map_err(rpc::error)
}

/// How often the page asks again while a Plex server is down.
const RECHECK_MS: i32 = 30_000;

#[component]
fn KindIcon(kind: LibraryKind) -> Element {
    match kind {
        LibraryKind::Movies => rsx! { icons::Film {} },
        LibraryKind::Shows => rsx! { icons::Tv {} },
        LibraryKind::Music => rsx! { icons::Music {} },
    }
}

#[component]
pub fn Libraries() -> Element {
    let mut list = use_resource(overview);
    let toast = use_signal(|| None::<Said>);
    let mut renaming = use_signal(|| None::<LibraryEntry>);
    let mut removing = use_signal(|| None::<LibraryEntry>);
    let mut mapping = use_signal(|| None::<LibraryEntry>);
    let mut menu = use_signal(|| None::<String>);
    // The row being dragged to a new place, and the row it is over.
    let mut dragged = use_signal(|| None::<usize>);
    let mut over = use_signal(|| None::<usize>);
    use_hook(move || {
        if let Some((ok, text)) = ARRIVING.with(|a| a.borrow_mut().take()) {
            say(toast, ok, text);
        }
    });
    // A server that is down is asked about again now and then, so its
    // card goes when it is back.
    use_hook(move || {
        spawn(async move {
            loop {
                sleep(RECHECK_MS).await;
                let any_down = matches!(
                    &*list.peek(),
                    Some(Ok(o)) if o.servers.iter().any(|s| !s.status.reachable)
                );
                if any_down {
                    list.restart();
                }
            }
        });
    });
    let current = list.read().clone();
    let synced = move |library: LibraryEntry, sync: SyncDirection| {
        spawn(async move {
            let done = async {
                let client = rpc::client().await?;
                client
                    .library_set_sync(library.id.clone(), sync)
                    .await
                    .map_err(rpc::error)
            }
            .await;
            match done {
                Ok(()) => say(toast, true, direction_changed(&library.name, sync)),
                Err(e) => say(toast, false, e.explain()),
            }
            list.restart();
        });
    };
    let moved = move |id: String, position: u32| {
        spawn(async move {
            let done = async {
                let client = rpc::client().await?;
                client.library_move(id, position).await.map_err(rpc::error)
            }
            .await;
            if let Err(e) = done {
                say(toast, false, e.explain());
            }
            list.restart();
        });
    };
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin", "Libraries" }
                if current.as_ref().is_some_and(|c| c.as_ref().is_ok_and(|o| !o.locked)) {
                    Link { class: "btn-add", to: Route::AddLibrary { back: String::new() },
                        icons::Plus { size: 16 }
                        "Add a library"
                    }
                }
            }
            super::playlists::LibrariesTabs { playlists: false }
            p { class: "lede m-0 max-w-[760px]",
                "What apps show, in this order. Each library comes from a source; today that's one library of a Plex server. Watch sync keeps each linked person's watched state in step with their own Plex account."
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(error)) => rsx! {
                    Alert { text: format!("Couldn't load the libraries: {}", error.explain()) }
                },
                Some(Ok(o)) => {
                    // By row, for a drop: the dragged row's library.
                    let order: Vec<String> = o.libraries.iter().map(|l| l.id.clone()).collect();
                    rsx! {
                    if o.locked {
                        div { class: "alert alert-info", role: "note",
                            icons::Lock { size: 16 }
                            span { "The server's config file sets the libraries, and is locked: change them there." }
                        }
                    }
                    for (name, libraries) in down(&o) {
                        DownCard {
                            key: "{name}",
                            name: name.clone(),
                            text: down_text(&libraries, o.libraries.len()),
                            on_check: move |_| list.restart(),
                        }
                    }
                    if o.libraries.is_empty() {
                        section { class: "card empty",
                            icons::Film { size: 40 }
                            b { "No libraries yet" }
                            span { "Apps have nothing to show until you add one." }
                            if !o.locked {
                                Link { class: "btn-dialog btn-dialog-primary", to: Route::AddLibrary { back: String::new() },
                                    icons::Plus { size: 16 }
                                    "Add a library"
                                }
                            }
                        }
                    } else {
                        section { class: "lib-table",
                            div { class: "lib-head",
                                span {}
                                span {}
                                span {}
                                span { "Library" }
                                span { "Items" }
                                span { "Paths" }
                                span { "Watch sync" }
                                span {}
                            }
                            for (i, library) in o.libraries.iter().cloned().enumerate() {
                                LibraryRow {
                                    key: "{library.id}",
                                    reachable: o.servers.iter().any(|s| s.id == library.server && s.status.reachable),
                                    first: i == 0,
                                    last: i + 1 == o.libraries.len(),
                                    locked: o.locked,
                                    dragging: dragged().is_some(),
                                    dragged: dragged() == Some(i),
                                    line: match (dragged(), over()) {
                                        (Some(from), Some(at)) if at == i => drop_line(from, i),
                                        _ => None,
                                    },
                                    on_drag_start: move |_| {
                                        menu.set(None);
                                        dragged.set(Some(i));
                                    },
                                    on_drag_over: move |_| {
                                        if over() != Some(i) {
                                            over.set(Some(i));
                                        }
                                    },
                                    on_drop: {
                                        let order = order.clone();
                                        move |_| {
                                            if let Some(from) = dragged.take()
                                                && let (Some(position), Some(id)) = (dropped_at(from, i), order.get(from))
                                            {
                                                moved(id.clone(), position);
                                            }
                                            over.set(None);
                                        }
                                    },
                                    on_drag_end: move |_| {
                                        dragged.set(None);
                                        over.set(None);
                                    },
                                    menu_open: menu() == Some(library.id.clone()),
                                    on_menu: {
                                        let id = library.id.clone();
                                        move |_| {
                                            let open = menu() == Some(id.clone());
                                            menu.set((!open).then(|| id.clone()));
                                        }
                                    },
                                    on_up: {
                                        let id = library.id.clone();
                                        move |_| moved(id.clone(), (i as u32).saturating_sub(1))
                                    },
                                    on_down: {
                                        let id = library.id.clone();
                                        move |_| moved(id.clone(), i as u32 + 1)
                                    },
                                    on_rename: {
                                        let library = library.clone();
                                        move |_| {
                                            menu.set(None);
                                            renaming.set(Some(library.clone()));
                                        }
                                    },
                                    on_paths: {
                                        let library = library.clone();
                                        move |_| {
                                            menu.set(None);
                                            mapping.set(Some(library.clone()));
                                        }
                                    },
                                    on_remove: {
                                        let library = library.clone();
                                        move |_| {
                                            menu.set(None);
                                            removing.set(Some(library.clone()));
                                        }
                                    },
                                    on_sync: {
                                        let library = library.clone();
                                        move |sync| synced(library.clone(), sync)
                                    },
                                    library,
                                }
                            }
                        }
                        if !o.chosen {
                            span { class: "hint",
                                "For now, every library of the first Plex server is served, under Plex's names. Your first change here keeps them all as your list of libraries, then makes the change."
                            }
                        }
                        div { class: "sync-grid",
                            super::plex::SyncPanel { toast }
                            super::plex::SyncExplained {}
                        }
                        // What paired servers share (P8-02): nothing with
                        // federation off.
                        super::paired::PairedServers {}
                    }
                    if let Some(library) = mapping() {
                        if let Some(server) = o.servers.iter().find(|s| s.id == library.server).cloned() {
                            PathMapping {
                                library,
                                server,
                                on_done: move |said: Option<(bool, String)>| {
                                    mapping.set(None);
                                    if let Some((ok, text)) = said {
                                        say(toast, ok, text);
                                        list.restart();
                                    }
                                },
                            }
                        }
                    }
                    }
                }
            }
        }
        if let Some(library) = renaming() {
            RenameLibrary {
                library,
                on_done: move |said: Option<(bool, String)>| {
                    renaming.set(None);
                    if let Some((ok, text)) = said {
                        say(toast, ok, text);
                        list.restart();
                    }
                },
            }
        }
        if let Some(library) = removing() {
            RemoveLibrary {
                library,
                on_done: move |said: Option<(bool, String)>| {
                    removing.set(None);
                    if let Some((ok, text)) = said {
                        say(toast, ok, text);
                        list.restart();
                    }
                },
            }
        }
        Toast { said: toast }
    }
}

/// WebLibrariesStates, 4: a Plex server that is down, and its libraries.
#[component]
fn DownCard(name: String, text: String, on_check: EventHandler<()>) -> Element {
    rsx! {
        section { class: "card max-w-[420px]",
            div { class: "card-body",
                div { class: "alert alert-error", role: "alert",
                    icons::CircleX {}
                    span { class: "flex flex-col gap-[3px]",
                        b { "{name} can't be reached" }
                        span { class: "text-ctp-text", "{text} Checked again automatically." }
                    }
                }
                button { class: "btn w-full", onclick: move |_| on_check.call(()),
                    icons::RotateCcw {}
                    "Check now"
                }
            }
        }
    }
}

#[component]
#[allow(clippy::too_many_arguments)]
fn LibraryRow(
    library: LibraryEntry,
    reachable: bool,
    first: bool,
    last: bool,
    locked: bool,
    /// A row, any of them, is being dragged; this one, and the line a
    /// drop on this one would land on.
    dragging: bool,
    dragged: bool,
    line: Option<DropLine>,
    menu_open: bool,
    on_drag_start: EventHandler<()>,
    on_drag_over: EventHandler<()>,
    on_drop: EventHandler<()>,
    on_drag_end: EventHandler<()>,
    on_menu: EventHandler<()>,
    on_up: EventHandler<()>,
    on_down: EventHandler<()>,
    on_rename: EventHandler<()>,
    on_paths: EventHandler<()>,
    on_remove: EventHandler<()>,
    on_sync: EventHandler<SyncDirection>,
) -> Element {
    let cell = path_cell(&library, reachable);
    let off = matches!(cell, PathCell::Unavailable(_));
    // Draggable only from the grip: a row that always were would take
    // the drag from its link and its select. The arrows stay for the
    // keyboard.
    let mut armed = use_signal(|| false);
    let class = [
        Some("lib-row"),
        off.then_some("off"),
        dragged.then_some("dragged"),
        match line {
            Some(DropLine::Above) => Some("drop-above"),
            Some(DropLine::Below) => Some("drop-below"),
            None => None,
        },
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let name = library.name.clone();
    rsx! {
        div {
            class: "{class}",
            draggable: if armed() && !locked { "true" } else { "false" },
            ondragstart: move |e: DragEvent| {
                let data = e.data_transfer();
                // Firefox starts no drag without data.
                let _ = data.set_data("text/plain", &name);
                data.set_effect_allowed("move");
                on_drag_start.call(());
            },
            ondragover: move |e: DragEvent| {
                if dragging {
                    // Without this, nothing may be dropped here.
                    e.prevent_default();
                    e.data_transfer().set_drop_effect("move");
                    on_drag_over.call(());
                }
            },
            ondrop: move |e: DragEvent| {
                if dragging {
                    e.prevent_default();
                    on_drop.call(());
                }
            },
            ondragend: move |_| {
                armed.set(false);
                on_drag_end.call(());
            },
            span {
                class: if locked { "lib-grip" } else { "lib-grip lib-grip-on" },
                aria_hidden: "true",
                title: if !locked { "Drag to reorder" },
                onpointerdown: move |_| armed.set(true),
                onpointerup: move |_| armed.set(false),
                icons::Grip {}
            }
            span { class: "lib-move",
                button {
                    aria_label: "Move {library.name} up",
                    disabled: first || locked,
                    onclick: move |_| on_up.call(()),
                    icons::ChevronUp {}
                }
                button {
                    aria_label: "Move {library.name} down",
                    disabled: last || locked,
                    onclick: move |_| on_down.call(()),
                    icons::ChevronDown {}
                }
            }
            span { class: "lib-icon", KindIcon { kind: library.kind } }
            span { class: "lib-what",
                // Its items (P3-16).
                Link {
                    class: "lib-name",
                    to: Route::CatalogLibrary {
                        library: library.id.clone(),
                        page: String::new(),
                        per: String::new(),
                        filter: String::new(),
                        kind: String::new(),
                        q: String::new(),
                        sort: String::new(),
                    },
                    "{library.name}"
                }
                span { class: "lib-source",
                    span { class: "plex-dot" }
                    "{source_line(&library)}"
                }
            }
            span { class: "lib-items", "{items_line(library.kind, library.items)}" }
            span { class: "lib-paths",
                match cell {
                    PathCell::Ok => rsx! { span { class: "state state-ok lib-state", icons::CircleCheck { size: 15 } "OK" } },
                    PathCell::Missing(n) => rsx! { span { class: "state state-warn lib-state", icons::Warning { size: 15 } "{n} missing" } },
                    PathCell::Unavailable(why) => rsx! {
                        span { class: "state state-bad lib-state", title: "{why}", icons::CircleX { size: 15 } "Unavailable: {why}" }
                    },
                    PathCell::Unknown => rsx! { span { class: "text-ctp-subtext0", "—" } },
                }
            }
            span { class: "lib-sync",
                select {
                    class: "sync-select",
                    aria_label: "Watch sync of {library.name}",
                    disabled: locked,
                    value: "{direction_value(library.sync)}",
                    onchange: move |e| {
                        if let Some(sync) = direction_of(&e.value()) {
                            on_sync.call(sync);
                        }
                    },
                    for d in DIRECTIONS {
                        option { value: "{direction_value(d)}", selected: d == library.sync, "{direction_name(d)}" }
                    }
                }
            }
            span { class: "relative",
                if !locked {
                    button {
                        class: "icon-button",
                        aria_label: "More for {library.name}",
                        aria_expanded: "{menu_open}",
                        onclick: move |_| on_menu.call(()),
                        icons::Ellipsis {}
                    }
                }
                if menu_open {
                    div { class: "row-menu", role: "menu",
                        button { role: "menuitem", onclick: move |_| on_rename.call(()), icons::Pencil {} "Rename" }
                        button { role: "menuitem", disabled: !reachable, onclick: move |_| on_paths.call(()), icons::Link {} "Path mapping" }
                        button { role: "menuitem", class: "danger", onclick: move |_| on_remove.call(()), icons::CircleX { size: 15 } "Remove" }
                    }
                }
            }
        }
    }
}

/// WebLibrariesStates, 1.
#[component]
fn RenameLibrary(library: LibraryEntry, on_done: EventHandler<Option<(bool, String)>>) -> Element {
    let name = use_signal(|| library.name.clone());
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let id = library.id.clone();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        let id = id.clone();
        spawn(async move {
            let renamed = async {
                let client = rpc::client().await?;
                client
                    .library_rename(id, name().trim().to_owned())
                    .await
                    .map_err(rpc::error)
            };
            match renamed.await {
                Ok(()) => on_done.call(Some((true, format!("Renamed to {}", name().trim())))),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let plex = library
        .plex_title
        .clone()
        .unwrap_or_else(|| library.name.clone());
    rsx! {
        div { class: "scrim",
            form { class: "dialog w-[440px]", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Pencil { size: 22 }
                    h1 { "Rename library" }
                }
                Field { label: "Name in Spoolframe", value: name }
                span { class: "hint", "Only Spoolframe's name changes. Plex keeps calling it {plex}." }
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

/// WebLibrariesStates, 2.
#[component]
fn RemoveLibrary(library: LibraryEntry, on_done: EventHandler<Option<(bool, String)>>) -> Element {
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let id = library.id.clone();
    let name = library.name.clone();
    let remove = move |_| {
        busy.set(true);
        let (id, name) = (id.clone(), name.clone());
        spawn(async move {
            let removed = async {
                let client = rpc::client().await?;
                client.library_remove(id).await.map_err(rpc::error)
            };
            match removed.await {
                Ok(()) => on_done.call(Some((true, format!("{name} removed · watch state kept")))),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::CircleX { size: 22 }
                    h1 { "Remove {library.name}?" }
                }
                p {
                    "Apps stop showing it. Watched state and progress are kept, so adding it back later picks up where everyone left off. Nothing changes in Plex."
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: remove, "Remove" }
                }
            }
        }
    }
}

async fn path_check(
    mappings: Vec<PathMappingEntry>,
    server: String,
    sections: Vec<String>,
) -> Result<Vec<LibraryPathCheck>, ApiError> {
    let client = rpc::client().await?;
    client
        .plex_path_try(mappings, Some(server), Some(sections))
        .await
        .map_err(rpc::error)
}

/// The row menu's "Path mapping": where the library's folder is here,
/// checked on a few files as it is typed.
#[component]
fn PathMapping(
    library: LibraryEntry,
    server: PlexServerEntry,
    on_done: EventHandler<Option<(bool, String)>>,
) -> Element {
    let title = library.plex_title.clone().unwrap_or_default();
    let root = library
        .paths
        .as_ref()
        .and_then(|p| p.roots.first().cloned())
        .unwrap_or_default();
    let saved = server.path_mapping.clone();
    let mut local = use_signal({
        let (saved, title, root) = (saved.clone(), title.clone(), root.clone());
        move || local_path(&saved, &title, &root)
    });
    let mut check = use_signal(|| library.paths.clone());
    let mut round = use_signal(|| 0u32);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let rules = {
        let (saved, title, root) = (saved.clone(), title.clone(), root.clone());
        move || with_path(&saved, &title, &root, &local())
    };
    let mut recheck = {
        let rules = rules.clone();
        let (server, section) = (server.id.clone(), library.section.clone());
        move || {
            round += 1;
            let n = round();
            check.set(None);
            let (rules, server, section) = (rules(), server.clone(), section.clone());
            spawn(async move {
                sleep(600).await;
                if round() != n {
                    return;
                }
                let found = path_check(rules, server, vec![section]).await;
                if round() == n {
                    match found {
                        Ok(mut checks) => check.set(checks.pop()),
                        Err(e) => error.set(Some(e.explain())),
                    }
                }
            });
        }
    };
    let save = {
        let server = server.id.clone();
        let name = library.name.clone();
        move |e: FormEvent| {
            e.prevent_default();
            busy.set(true);
            let (rules, server, name) = (rules(), server.clone(), name.clone());
            spawn(async move {
                let set = async {
                    let client = rpc::client().await?;
                    client
                        .plex_server_set_paths(server, rules)
                        .await
                        .map_err(rpc::error)
                };
                match set.await {
                    Ok(()) => on_done.call(Some((true, format!("Path saved for {name}")))),
                    Err(e) => error.set(Some(e.explain())),
                }
                busy.set(false);
            });
        }
    };
    rsx! {
        div { class: "scrim",
            form { class: "dialog dialog-wide", role: "dialog", onsubmit: save,
                div { class: "dialog-head",
                    icons::Link { size: 22 }
                    h1 { "Path mapping for {library.name}" }
                }
                if root.is_empty() {
                    p { class: "m-0 text-sm text-ctp-subtext1",
                        "Plex lists no folder for this library, so there is nothing to map."
                    }
                } else {
                    div { class: "flex flex-col gap-1.5",
                        span { class: "text-xs font-bold uppercase tracking-[0.05em] text-ctp-subtext0", "On Plex" }
                        span { class: "plex-path", "{root}" }
                    }
                    label { class: "field",
                        "Path on this server"
                        input {
                            class: "font-mono",
                            value: "{local}",
                            oninput: move |e| {
                                local.set(e.value());
                                recheck();
                            },
                        }
                    }
                    match check() {
                        None => rsx! { CheckingState {} },
                        Some(c) => rsx! { SampleLine { check: c } },
                    }
                    span { class: "hint", "Saved as one mapping for this library's folder, on {server.name}." }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy() || root.is_empty(), "Save" }
                }
            }
        }
    }
}

#[component]
fn CheckingState() -> Element {
    rsx! {
        span { class: "flex items-center gap-1.5 text-[13px] font-semibold text-ctp-blue", role: "status",
            span { class: "flex animate-spin", icons::Loader { size: 14 } }
            "Checking…"
        }
    }
}

#[component]
fn SampleLine(check: LibraryPathCheck) -> Element {
    let (ok, line) = sample_line(&check);
    rsx! {
        span { class: if ok { "state state-ok lib-state" } else { "state state-warn lib-state" },
            if ok { icons::CircleCheck { size: 15 } } else { icons::Warning { size: 15 } }
            "{line}"
        }
    }
}

/// Which Plex server the add flow is on.
#[derive(Debug, Clone, PartialEq)]
enum Pick {
    Nothing,
    /// One added already, by id.
    Added(String),
    /// One found on the LAN, by URL.
    Found(String),
    /// One the caller's linked Plex account reaches (P3-12), by URL.
    Account(String),
    /// One typed.
    Typed,
}

/// One section of the chosen server, as the add flow's table has it.
#[derive(Debug, Clone, PartialEq)]
struct AddRow {
    section: String,
    title: String,
    line: String,
    /// The library it is already, if it is one.
    library: Option<String>,
    checked: bool,
    name: String,
    root: String,
    path: String,
    check: Option<LibraryPathCheck>,
}

/// Where the add flow goes back to: Libraries, or the page it came from.
fn back_to(back: &str) -> Route {
    match back {
        "wizard" => Route::Wizard {
            step: "plex".into(),
        },
        "settings" => Route::Settings { tab: "plex".into() },
        _ => Route::Libraries {},
    }
}

async fn probe(url: String, token: Option<String>) -> Result<PlexStatus, ApiError> {
    let client = rpc::client().await?;
    client.plex_probe(url, token).await.map_err(rpc::error)
}

/// The rows for `status`'s sections, the ones that are libraries checked
/// and fixed. On a server none of whose sections is a library yet, every
/// one starts checked.
fn rows_of(status: &PlexStatus) -> Vec<AddRow> {
    let fresh = status.libraries.iter().all(|s| s.library.is_none());
    status
        .libraries
        .iter()
        .map(|s| AddRow {
            section: s.section.clone(),
            title: s.title.clone(),
            line: section_line(s),
            library: s.library.clone(),
            checked: fresh || s.library.is_some(),
            name: s.title.clone(),
            root: String::new(),
            path: String::new(),
            check: None,
        })
        .collect()
}

/// The server's mappings with every checked row's folder where its path
/// says.
fn rules_of(saved: &[PathMappingEntry], rows: &[AddRow]) -> Vec<PathMappingEntry> {
    rows.iter()
        .filter(|r| r.checked && r.library.is_none() && !r.root.is_empty())
        .fold(saved.to_vec(), |rules, r| {
            with_path(&rules, &r.title, &r.root, &r.path)
        })
}

/// WebLibraryAdd: Source, Plex server, Account, Libraries.
#[component]
pub fn AddLibrary(back: String) -> Element {
    let nav = navigator();
    let known = use_resource(overview);
    let found = use_resource(|| async {
        let client = rpc::client().await?;
        client.plex_discover().await.map_err(rpc::error)
    });
    // The servers the admin's linked Plex account reaches (P3-12), or the
    // account signed in with here or in the wizard (P3-18); none without
    // either. Asked again after each sign-in.
    let mut signed_in = use_signal(|| 0u32);
    let from_account = use_resource(move || async move {
        let _ = signed_in();
        let client = rpc::client().await?;
        client.plex_account_servers().await.map_err(rpc::error)
    });
    // Signing in with Plex here, for its servers' own tokens: the first
    // server's token stays as it is.
    let account = use_signal(|| Account::Idle);
    let sign_in = move |_| {
        sign_in_with_plex(account, false, None, move || signed_in += 1);
    };
    let linked = use_resource(|| async {
        let client = rpc::client().await?;
        client.plex_link().await.map_err(rpc::error)
    });
    let mut pick = use_signal(|| Pick::Nothing);
    let mut typed = use_signal(String::new);
    let mut pasting = use_signal(|| false);
    let pasted = use_signal(String::new);
    let mut probed = use_signal(|| None::<Result<PlexStatus, ApiError>>);
    let mut probes = use_signal(|| 0u32);
    // A server this flow added, to take back out on Cancel.
    let mut added_here = use_signal(|| None::<String>);
    // The server whose libraries are listed, and its saved mappings.
    let mut server = use_signal(|| None::<(String, String, Vec<PathMappingEntry>)>);
    let mut rows = use_signal(Vec::<AddRow>::new);
    let mut unsupported = use_signal(Vec::<String>::new);
    let mut round = use_signal(|| 0u32);
    let mut error = use_signal(|| None::<String>);
    let toast = use_signal(|| None::<Said>);
    let mut busy = use_signal(|| false);

    let current = known.read().clone();

    // Check the checked rows' paths: all of them, or (after a keystroke)
    // the one changed, a moment after typing stops.
    let mut check_rows = move |only: Option<String>| {
        let Some((id, _, saved)) = server.peek().clone() else {
            return;
        };
        round += 1;
        let n = round();
        let sections: Vec<String> = rows
            .peek()
            .iter()
            .filter(|r| r.library.is_none() && only.as_ref().is_none_or(|o| *o == r.section))
            .map(|r| r.section.clone())
            .collect();
        if sections.is_empty() {
            return;
        }
        rows.with_mut(|rs| {
            for r in rs.iter_mut().filter(|r| sections.contains(&r.section)) {
                r.check = None;
            }
        });
        spawn(async move {
            if only.is_some() {
                sleep(600).await;
                if round() != n {
                    return;
                }
            }
            let rules = rules_of(&saved, &rows.peek());
            match path_check(rules, id, sections).await {
                Ok(checks) => rows.with_mut(|rs| {
                    for c in checks {
                        if let Some(r) = rs.iter_mut().find(|r| r.title == c.library) {
                            if r.root.is_empty() {
                                r.root = c.roots.first().cloned().unwrap_or_default();
                                r.path = c
                                    .suggestion
                                    .as_ref()
                                    .filter(|_| c.ok < c.checked)
                                    .map(|s| local_path(std::slice::from_ref(s), &r.title, &r.root))
                                    .unwrap_or_else(|| local_path(&saved, &r.title, &r.root));
                            }
                            r.check = Some(c);
                        }
                    }
                }),
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };

    // List the libraries of a server that is added (now, or before).
    let mut list_server =
        move |id: String, name: String, saved: Vec<PathMappingEntry>, status: PlexStatus| {
            unsupported.set(status.unsupported.iter().map(|u| u.title.clone()).collect());
            rows.set(rows_of(&status));
            server.set(Some((id, name, saved)));
            check_rows(None);
        };

    // The first server added says where the token kept before it goes
    // (finding 47): from an old session, "Confirm it's you", then the
    // same add again.
    let confirming = use_signal(|| None::<String>);
    let mut again = use_signal(|| None::<(String, Option<String>)>);
    // A new server: once it answers, it is added (taken back out on
    // Cancel), so its libraries can be checked before they are.
    let mut take_new = move |url: String, token: Option<String>| {
        probes += 1;
        let n = probes();
        probed.set(None);
        rows.set(Vec::new());
        server.set(None);
        error.set(None);
        spawn(async move {
            // Another one this flow added goes first.
            let held = added_here.peek().clone();
            if let Some(id) = held {
                let client = rpc::client().await;
                if let Ok(client) = client {
                    let _ = client.plex_server_remove(id).await;
                }
                added_here.set(None);
            }
            if url.trim().is_empty() {
                return;
            }
            sleep(400).await;
            if probes() != n {
                return;
            }
            let status = probe(url.clone(), token.clone()).await;
            if probes() != n {
                return;
            }
            let reachable = status.as_ref().is_ok_and(|s| s.reachable);
            probed.set(Some(status));
            if !reachable {
                return;
            }
            let added = async {
                let client = rpc::client().await?;
                client
                    .plex_server_add(url.clone(), token.clone())
                    .await
                    .map_err(rpc::error)
            };
            match added.await {
                Ok(entry) => {
                    added_here.set(Some(entry.id.clone()));
                    list_server(entry.id, entry.name, entry.path_mapping, entry.status);
                }
                Err(e) if asks_to_confirm(&e, confirming, "adding the first Plex server") => {
                    again.set(Some((url, token)));
                }
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };

    let back_route = back_to(&back);
    let cancel = {
        let back_route = back_route.clone();
        move |_| {
            let back_route = back_route.clone();
            spawn(async move {
                let held = added_here.peek().clone();
                if let Some(id) = held
                    && let Ok(client) = rpc::client().await
                {
                    let _ = client.plex_server_remove(id).await;
                }
                nav.push(back_route);
            });
        }
    };
    let chosen: Vec<AddRow> = rows()
        .into_iter()
        .filter(|r| r.checked && r.library.is_none())
        .collect();
    // A chosen library none of whose files is here holds the add up: it
    // would play nothing.
    let refused = refusals(chosen.iter().map(|r| r.check.as_ref()));
    let add = {
        let back_route = back_route.clone();
        let chosen = chosen.clone();
        move |_| {
            let Some((id, name, saved)) = server() else {
                return;
            };
            let chosen = chosen.clone();
            let back_route = back_route.clone();
            busy.set(true);
            spawn(async move {
                let rules = rules_of(&saved, &rows.peek());
                let client = match rpc::client().await {
                    Ok(client) => client,
                    Err(e) => {
                        error.set(Some(e.explain()));
                        busy.set(false);
                        return;
                    }
                };
                if rules != saved
                    && let Err(e) = client.plex_server_set_paths(id.clone(), rules).await
                {
                    error.set(Some(rpc::error(e).explain()));
                    busy.set(false);
                    return;
                }
                let mut added = 0;
                let mut failed = None;
                for r in &chosen {
                    let named = (r.name.trim() != r.title).then(|| r.name.trim().to_owned());
                    match client
                        .library_add(id.clone(), r.section.clone(), named)
                        .await
                    {
                        Ok(_) => added += 1,
                        Err(e) => {
                            failed.get_or_insert(format!(
                                "Couldn't add {}: {}",
                                r.title,
                                rpc::error(e).explain()
                            ));
                        }
                    }
                }
                busy.set(false);
                match failed {
                    Some(text) if added == 0 => error.set(Some(text)),
                    Some(text) => {
                        say_next(false, text);
                        nav.push(back_route);
                    }
                    None => {
                        say_next(true, added_text(added, &name));
                        nav.push(back_route);
                    }
                }
            });
        }
    };

    let Some(Ok(o)) = current else {
        return rsx! {
            match current {
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                _ => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
            }
        };
    };
    // Servers found on the LAN that are not added already.
    let added_urls: Vec<String> = o
        .servers
        .iter()
        .filter_map(|s| s.status.url.clone())
        .collect();
    let found_list = found.read().clone();
    let new_found: Vec<_> = match &found_list {
        Some(Ok(list)) => list
            .iter()
            .filter(|f| {
                !added_urls
                    .iter()
                    .any(|u| u.trim_end_matches('/') == f.url.trim_end_matches('/'))
                    || added_here()
                        .is_some_and(|_| matches!(pick(), Pick::Found(ref p) if *p == f.url))
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    // The account's servers not added already, and not found on the LAN.
    let account_servers: Vec<_> = match &*from_account.read() {
        Some(Ok(list)) => list
            .iter()
            .filter(|s| {
                s.added.is_none()
                    || matches!(pick(), Pick::Account(ref u) if Some(u) == s.url.as_ref())
            })
            .filter(|s| {
                s.url.as_ref().is_some_and(|u| {
                    !new_found
                        .iter()
                        .any(|f| f.url.trim_end_matches('/') == u.trim_end_matches('/'))
                })
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let linked_name = match &*linked.read() {
        Some(Ok(me)) => me
            .link
            .as_ref()
            .filter(|l| !l.by_config)
            .and_then(|l| l.email.clone().or_else(|| l.username.clone())),
        _ => None,
    };
    let signed_in_as = match account() {
        Account::Linked(who) => who,
        _ => None,
    };
    // Whose account a server from it is added with.
    let account_name = linked_name
        .clone()
        .map(|who| format!("Uses your linked Plex account, {who}"))
        .or_else(|| {
            signed_in_as
                .clone()
                .map(|who| format!("Uses the Plex account signed in as {who}"))
        });
    // No account lists servers yet: offer to sign in with Plex (P3-18).
    let no_account = matches!(&*from_account.read(), Some(Err(_)));
    let signing_in = !matches!(account(), Account::Idle | Account::Linked(_));
    let nothing_from_account = account_servers.is_empty() && from_account.read().is_some();
    let is_new = matches!(pick(), Pick::Found(_) | Pick::Account(_) | Pick::Typed);
    let server_done = server().is_some();
    let needs_token = probed()
        .as_ref()
        .and_then(|p| p.as_ref().ok())
        .is_some_and(|s| !s.reachable && s.error.as_deref().is_some_and(|e| e.contains("token")));
    let token = move || {
        let t = pasted().trim().to_owned();
        (pasting() && !t.is_empty()).then_some(t)
    };
    let url_now = move || match pick() {
        Pick::Found(url) | Pick::Account(url) => url,
        Pick::Typed => typed(),
        _ => String::new(),
    };
    let server_name = server().map(|(_, name, _)| name).unwrap_or_default();
    let steps = [
        ("Source", true),
        ("Plex server", server_done),
        ("Account", server_done),
        ("Libraries", false),
    ];
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            ConfirmItsYou {
                asking: confirming,
                on_confirmed: move |_| {
                    if let Some((url, token)) = again.take() {
                        take_new(url, token);
                    }
                },
            }
            Link { class: "back-link", to: back_route.clone(),
                icons::ChevronLeft {}
                if back == "wizard" { "Setup" } else if back == "settings" { "Settings" } else { "Libraries" }
            }
            div { class: "page-head",
                h1 { class: "page-title page-title-admin", "Add a library" }
            }
            div { class: "step-chips",
                for (i, (label, done)) in steps.iter().enumerate() {
                    span { key: "{label}", class: if *done { "step-chip step-chip-done" } else { "step-chip" },
                        if *done { icons::Check {} } else { "{i + 1} " }
                        "{label}"
                    }
                }
            }
            div { class: "add-grid",
                div { class: "flex flex-col gap-4",
                    div { class: "flex flex-col gap-2.5",
                        h2 { class: "add-h2", "Source" }
                        button { class: "choice choice-here", r#type: "button",
                            span { class: "radio" }
                            span { class: "choice-what",
                                b { "Plex" }
                                "One library of a Plex Media Server"
                            }
                        }
                        button { class: "choice", r#type: "button", disabled: true,
                            span { class: "radio" }
                            span { class: "choice-what",
                                b { "Folder on this server" }
                                "Spoolframe's own library · in a later version"
                            }
                        }
                    }
                    div { class: "flex flex-col gap-2.5",
                        h2 { class: "add-h2", "Plex server" }
                        for s in o.servers.iter().filter(|s| added_here() != Some(s.id.clone())).cloned() {
                            button {
                                key: "{s.id}",
                                r#type: "button",
                                class: if pick() == Pick::Added(s.id.clone()) { "choice choice-here" } else { "choice" },
                                disabled: !s.status.reachable,
                                onclick: {
                                    let s = s.clone();
                                    move |_| {
                                        pick.set(Pick::Added(s.id.clone()));
                                        probed.set(None);
                                        pasting.set(false);
                                        take_new(String::new(), None);
                                        list_server(s.id.clone(), s.name.clone(), s.path_mapping.clone(), s.status.clone());
                                    }
                                },
                                span { class: "radio" }
                                span { class: "choice-what",
                                    b { "{s.name}" }
                                    if s.status.reachable {
                                        {format!("Already added · {}", super::count_of(o.libraries.iter().filter(|l| l.server == s.id).count() as u64, "library", "libraries"))}
                                    } else {
                                        "Already added · can't be reached now"
                                    }
                                }
                            }
                        }
                        match found_list {
                            None => rsx! {
                                div { class: "flex items-center gap-2 text-sm text-ctp-subtext0", role: "status",
                                    span { class: "flex animate-spin", icons::Loader {} }
                                    "Looking for Plex on this network…"
                                }
                            },
                            _ => rsx! {
                                for f in new_found {
                                    button {
                                        key: "{f.url}",
                                        r#type: "button",
                                        class: if pick() == Pick::Found(f.url.clone()) { "choice choice-here" } else { "choice" },
                                        onclick: {
                                            let url = f.url.clone();
                                            move |_| {
                                                pick.set(Pick::Found(url.clone()));
                                                take_new(url.clone(), token());
                                            }
                                        },
                                        span { class: "radio" }
                                        span { class: "choice-what",
                                            b { "{f.name}" }
                                            "Found on this network · {host_port(&f.url)}"
                                            if remote(&f.url, None) {
                                                RemoteNote {}
                                            }
                                        }
                                        if pick() == Pick::Found(f.url.clone()) {
                                            ProbeBadge { probed: probed() }
                                        }
                                    }
                                }
                            },
                        }
                        for a in account_servers {
                            if let Some(url) = a.url.clone() {
                                button {
                                    key: "{a.machine_id}",
                                    r#type: "button",
                                    class: if pick() == Pick::Account(url.clone()) { "choice choice-here" } else { "choice" },
                                    onclick: {
                                        let url = url.clone();
                                        move |_| {
                                            pick.set(Pick::Account(url.clone()));
                                            take_new(url.clone(), token());
                                        }
                                    },
                                    span { class: "radio" }
                                    span { class: "choice-what",
                                        b { "{a.name}" }
                                        if remote(&url, Some(a.local)) {
                                            // Its plex.direct name is too long to say.
                                            "From your Plex account"
                                            RemoteNote {}
                                        } else {
                                            "From your Plex account · {host_port(&url)}"
                                        }
                                    }
                                    if pick() == Pick::Account(url.clone()) {
                                        ProbeBadge { probed: probed() }
                                    }
                                }
                            }
                        }
                        if signing_in {
                            AccountPanel {
                                account: account(),
                                token_set: false,
                                on_sign_in: move |_| sign_in(()),
                                on_cancel: move |_| {
                                    let mut account = account;
                                    account.set(Account::Idle);
                                },
                            }
                        } else if let Some(who) = signed_in_as.clone().filter(|_| nothing_from_account) {
                            span { class: "text-[13px] text-ctp-subtext0",
                                "The Plex account signed in as {who} has no other servers to add."
                            }
                        } else if no_account && signed_in_as.is_none() {
                            div { class: "account-line",
                                span { class: "flex-grow text-[13px] text-ctp-subtext0",
                                    "Sign in with Plex to list your account's servers. Each is added with its own token; the other servers keep theirs."
                                }
                                button { class: "btn", r#type: "button", onclick: move |_| sign_in(()), "Sign in with Plex" }
                            }
                        }
                        button {
                            r#type: "button",
                            class: if pick() == Pick::Typed { "choice choice-here" } else { "choice" },
                            onclick: move |_| {
                                pick.set(Pick::Typed);
                                take_new(typed(), token());
                            },
                            span { class: "radio" }
                            span { class: "choice-what",
                                b { "Type an address" }
                                if pick() == Pick::Typed && remote(&typed(), None) {
                                    RemoteNote {}
                                }
                            }
                            if pick() == Pick::Typed {
                                ProbeBadge { probed: probed() }
                            }
                        }
                        if pick() == Pick::Typed {
                            label { class: "field",
                                "Address"
                                input {
                                    r#type: "url",
                                    placeholder: "http://host:32400",
                                    value: "{typed}",
                                    oninput: move |e| {
                                        typed.set(e.value());
                                        take_new(e.value(), token());
                                    },
                                }
                            }
                        }
                        span { class: "text-xs text-ctp-subtext0", "{FILES_HERE}" }
                        if let Some(Ok(p)) = probed().filter(|_| is_new) {
                            if !p.reachable && !needs_token {
                                div { class: "alert alert-error", role: "alert",
                                    icons::CircleX {}
                                    span {
                                        b { "Plex cannot be reached" }
                                        {p.error.clone().unwrap_or_default()}
                                    }
                                }
                            }
                        }
                    }
                    if is_new {
                        div { class: "flex flex-col gap-2.5",
                            h2 { class: "add-h2", "Account" }
                            // Another server's token is never used for this
                            // one (PX-10): the account's, or one pasted.
                            if pasting()
                                || needs_token
                                || !(matches!(pick(), Pick::Account(_)) && account_name.is_some())
                            {
                                div { class: "panel",
                                    if needs_token {
                                        span { "This Plex server needs a token the server does not have for it." }
                                    }
                                    // In a form of its own, as browsers expect
                                    // of a password field; Enter does nothing.
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
                                    button {
                                        class: "btn btn-teal h-9",
                                        r#type: "button",
                                        disabled: pasted().trim().is_empty(),
                                        onclick: move |_| {
                                            pasting.set(true);
                                            take_new(url_now(), token());
                                        },
                                        "Check token"
                                    }
                                }
                            } else if let (Pick::Account(_), Some(line)) = (pick(), account_name.clone()) {
                                div { class: "account-line",
                                    span { class: "text-ctp-green", icons::CircleCheck {} }
                                    span { class: "flex-grow", "{line}" }
                                    button { class: "link-button", r#type: "button", onclick: move |_| pasting.set(true), "Paste a token" }
                                }
                            }
                        }
                    }
                }
                if server_done {
                    section { class: "add-panel",
                        div { class: "flex items-baseline justify-between gap-3",
                            h2 { class: "m-0 text-base font-bold", "Libraries on {server_name}" }
                            span { class: "text-xs text-ctp-subtext0", "Name and path can be changed later" }
                        }
                        div { class: "add-head",
                            span {}
                            span { "On Plex" }
                            span { "Name in Spoolframe" }
                            span { "Path on this server" }
                            span { "5 sample files" }
                        }
                        for (i, r) in rows().into_iter().enumerate() {
                            div { key: "{r.section}", class: "add-row",
                                button {
                                    r#type: "button",
                                    class: if r.checked { "box box-on" } else { "box" },
                                    role: "checkbox",
                                    aria_checked: "{r.checked}",
                                    aria_label: "Add {r.title}",
                                    disabled: r.library.is_some(),
                                    onclick: {
                                        let section = r.section.clone();
                                        move |_| {
                                            rows.with_mut(|rs| rs[i].checked = !rs[i].checked);
                                            if rows.peek()[i].checked && rows.peek()[i].check.is_none() {
                                                check_rows(Some(section.clone()));
                                            }
                                        }
                                    },
                                    if r.checked { icons::Check {} }
                                }
                                span { class: "flex flex-col gap-0.5",
                                    b { class: "text-sm", "{r.title}" }
                                    span { class: "text-xs text-ctp-subtext0", "{r.line}" }
                                }
                                if r.library.is_some() {
                                    span { class: "text-[13px] text-ctp-subtext0 add-wide", "A library already" }
                                } else if r.checked {
                                    input {
                                        aria_label: "Name in Spoolframe",
                                        value: "{r.name}",
                                        oninput: move |e| rows.with_mut(|rs| rs[i].name = e.value()),
                                    }
                                    input {
                                        class: "font-mono",
                                        aria_label: "Path on this server",
                                        value: "{r.path}",
                                        oninput: {
                                            let section = r.section.clone();
                                            move |e: FormEvent| {
                                                rows.with_mut(|rs| rs[i].path = e.value());
                                                check_rows(Some(section.clone()));
                                            }
                                        },
                                    }
                                    match r.check.clone() {
                                        None => rsx! { CheckingState {} },
                                        Some(c) => rsx! { SampleLine { check: c } },
                                    }
                                } else {
                                    span { class: "add-wide" }
                                }
                            }
                        }
                        for title in unsupported() {
                            div { key: "{title}", class: "add-row off",
                                button { class: "box", r#type: "button", role: "checkbox", aria_checked: "false", disabled: true }
                                span { class: "flex flex-col gap-0.5",
                                    b { class: "text-sm", "{title}" }
                                    span { class: "text-xs text-ctp-subtext0", "Spoolframe does not serve these" }
                                }
                                span { class: "add-wide" }
                            }
                        }
                        for text in refused.clone() {
                            Alert { key: "{text}", text }
                        }
                        if let Some(text) = error() {
                            Alert { text }
                        }
                        div { class: "flex justify-end gap-2.5 pt-3.5",
                            button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: cancel, "Cancel" }
                            button {
                                class: "btn-dialog btn-dialog-primary",
                                r#type: "button",
                                disabled: busy() || chosen.is_empty() || !refused.is_empty(),
                                onclick: add,
                                "{add_label(chosen.len())}"
                            }
                        }
                    }
                } else if let Some(text) = error() {
                    Alert { text }
                }
            }
        }
        Toast { said: toast }
    }
}

/// On a Plex server away from this machine's network.
#[component]
pub(super) fn RemoteNote() -> Element {
    rsx! {
        span { class: "state state-warn", "{REMOTE_NOTE}" }
    }
}

/// "Reachable" on a chosen server, or that it is being asked.
#[component]
fn ProbeBadge(probed: Option<Result<PlexStatus, ApiError>>) -> Element {
    match probed {
        None => rsx! {
            span { class: "flex animate-spin text-ctp-blue", icons::Loader { size: 14 } }
        },
        Some(Ok(p)) if p.reachable => rsx! {
            span { class: "state state-ok lib-state", icons::CircleCheck { size: 15 } "Reachable" }
        },
        Some(_) => rsx! {
            span { class: "state state-bad lib-state", icons::CircleX { size: 15 } "Not reachable" }
        },
    }
}
