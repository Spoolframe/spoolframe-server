//! Server › Libraries › a library (P3-16): its items in a paged table,
//! as the WebCatalog, WebCatalogMissing, WebCatalogStates and
//! WebCatalogPhone boards draw it. Read-only; matches are fixed in Plex.
//!
//! Where the page is (its page, page size, filter, kind and search) is in
//! the address, so a reload or a shared link opens the same rows. The
//! columns an admin adds are theirs, kept in this browser
//! (`localStorage`): a per-viewer preference, not the server's business.

use std::collections::HashSet;

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, ArtworkKind, CatalogFilter, CatalogLibrary as LibraryView, CatalogPage,
    CatalogQuery, CatalogRow, Item, LibraryKind, PathsChecked, error_code,
};

use super::admin::sleep;
use super::{Route, icons, now, rpc};
use crate::catalog::{
    Columns, FILTERS, FileCell, PER_PAGE, View, agent_names, file_cell, file_text, filter_banner,
    filter_count, filter_label, filter_name, first_file, header_line, item_kind, kind_label,
    kind_name, kind_plural, nothing_text, opens, pager, pager_text, row_title, searched_text,
    selectable, subtitle, top_count, unit, year,
};
use crate::logic::{ApiError, date, thousands};

/// Where the added columns are kept in this browser.
const COLUMNS_KEY: &str = "spoolframe.catalog.columns";

/// How often a page asks again while a path check runs.
const CHECKING_MS: i32 = 2_000;

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn load_columns() -> Columns {
    storage()
        .and_then(|s| s.get_item(COLUMNS_KEY).ok().flatten())
        .map(|kept| Columns::load(&kept))
        .unwrap_or_default()
}

fn keep_columns(columns: Columns) {
    if let Some(s) = storage() {
        let _ = s.set_item(COLUMNS_KEY, &columns.store());
    }
}

/// The address of artwork `id` at a size the server makes.
pub(super) fn artwork_src(id: &str, size: &str) -> String {
    let id = js_sys::encode_uri_component(id);
    format!("/catalog/artwork?id={id}&size={size}")
}

/// The image a row shows: a poster, or an episode's still.
pub(super) fn thumb_of(item: &Item) -> Option<String> {
    let artwork = match item {
        Item::Movie(v) => &v.artwork,
        Item::Show(v) => &v.artwork,
        Item::Season(v) => &v.artwork,
        Item::Episode(v) => &v.artwork,
        Item::Artist(v) => &v.artwork,
        Item::Album(v) => &v.artwork,
        Item::Track(_) => return None,
    };
    let first = match item {
        Item::Episode(_) => ArtworkKind::Thumb,
        _ => ArtworkKind::Poster,
    };
    artwork
        .iter()
        .find(|a| a.kind == first)
        .or_else(|| artwork.first())
        .map(|a| a.id.to_string())
}

/// The route to `view` of `library`.
fn route_to(library: &str, view: &View) -> Route {
    Route::CatalogLibrary {
        library: library.to_owned(),
        page: if view.page > 1 {
            view.page.to_string()
        } else {
            String::new()
        },
        per: if view.per != PER_PAGE[0] {
            view.per.to_string()
        } else {
            String::new()
        },
        filter: view.filter.map(filter_name).unwrap_or_default().to_owned(),
        kind: view.kind.map(kind_name).unwrap_or_default().to_owned(),
        q: view.text.clone(),
    }
}

async fn load_library(id: String) -> Result<LibraryView, ApiError> {
    let client = rpc::client().await?;
    client.catalog_library(id).await.map_err(rpc::error)
}

async fn load_rows(library: String, view: View) -> Result<CatalogPage, ApiError> {
    let client = rpc::client().await?;
    let text = view.text.trim();
    client
        .catalog_rows(CatalogQuery {
            library,
            kind: view.kind,
            text: (!text.is_empty()).then(|| text.to_owned()),
            filter: view.filter,
            offset: view.offset(),
            limit: view.per,
        })
        .await
        .map_err(rpc::error)
}

#[component]
pub(super) fn KindIcon(kind: LibraryKind, #[props(default = 22)] size: u32) -> Element {
    match kind {
        LibraryKind::Movies => rsx! { icons::Film { size } },
        LibraryKind::Shows => rsx! { icons::Tv { size } },
        LibraryKind::Music => rsx! { icons::Music { size } },
    }
}

/// The board's "Only admins can see the catalog": what a consumer who
/// opens a catalog link sees.
#[component]
pub(super) fn Forbidden() -> Element {
    rsx! {
        section { class: "card empty max-w-[460px] self-center",
            span { class: "text-ctp-yellow", icons::Lock { size: 26 } }
            b { "Only admins can see the catalog" }
            span { "Ask an admin if you think you need it." }
            Link { class: "btn", to: Route::Profile {}, "Go to My profile" }
        }
    }
}

/// The board's loading state: rows of grey bars.
#[component]
pub(super) fn Skeleton(#[props(default = 6)] rows: usize) -> Element {
    rsx! {
        section { class: "card cat-skeleton", role: "status", aria_label: "Loading",
            for i in 0..rows {
                div { key: "{i}", class: "cat-skeleton-row",
                    span { class: "cat-skeleton-thumb" }
                    span { class: "flex flex-grow flex-col gap-2",
                        span { class: "cat-skeleton-bar", style: "width: {60 - (i % 3) * 12}%" }
                        span { class: "cat-skeleton-bar cat-skeleton-thin", style: "width: {40 - (i % 2) * 8}%" }
                    }
                }
            }
        }
    }
}

/// A Plex server that cannot be reached: the server keeps no copy of
/// the catalog, so there is nothing older to show.
#[component]
pub(super) fn Unreachable(error: ApiError, on_retry: EventHandler<()>) -> Element {
    rsx! {
        div { class: "alert alert-warning", role: "alert",
            icons::Warning {}
            span { class: "flex flex-col gap-[3px]",
                b { "Plex can't be reached" }
                span { class: "text-ctp-text",
                    "Spoolframe reads the catalog from Plex as it is asked and keeps no copy of it, so there is nothing to show until Plex answers. "
                    "{error.explain()}"
                }
                span {
                    button { class: "btn mt-1", onclick: move |_| on_retry.call(()), icons::RotateCcw {} "Try again" }
                }
            }
        }
    }
}

/// What a failed load shows: FORBIDDEN, Plex down, or the error.
#[component]
fn Failed(error: ApiError, on_retry: EventHandler<()>) -> Element {
    match error.code {
        error_code::FORBIDDEN => rsx! { Forbidden {} },
        error_code::UNAVAILABLE => rsx! { Unreachable { error, on_retry } },
        _ => rsx! { super::Alert { text: error.explain() } },
    }
}

#[component]
pub fn CatalogLibrary(
    library: String,
    page: String,
    per: String,
    filter: String,
    kind: String,
    q: String,
) -> Element {
    let view = View::parse(&page, &per, &filter, &kind, &q);
    let nav = navigator();
    let mut header = use_resource(use_reactive((&library,), |(library,)| {
        load_library(library)
    }));
    let mut rows = use_resource(use_reactive((&library, &view), |(library, view)| {
        load_rows(library, view)
    }));
    let mut columns = use_signal(load_columns);
    let mut menu = use_signal(|| false);
    let opened = use_signal(HashSet::<String>::new);
    let mut search = use_signal(|| view.text.clone());
    // The box follows the address: back and forward change it.
    use_effect(use_reactive((&q,), move |(q,)| search.set(q)));

    // While the path check runs, ask again now and then: its counts, and
    // under Missing files, its rows.
    let checking = matches!(
        &*header.read(),
        Some(Ok(l)) if matches!(l.paths, PathsChecked::Checking { .. } | PathsChecked::NotChecked)
    );
    use_effect(use_reactive((&checking,), move |(checking,)| {
        if checking {
            spawn(async move {
                sleep(CHECKING_MS).await;
                header.restart();
                rows.restart();
            });
        }
    }));

    let go = {
        let library = library.clone();
        move |next: View| {
            nav.push(route_to(&library, &next));
        }
    };
    let current_header = header.read().clone();
    let current_rows = rows.read().clone();
    let shown_columns = columns().for_filter(view.filter);

    let Some(found) = current_header else {
        return rsx! { Skeleton {} };
    };
    let lib = match found {
        Ok(lib) => lib,
        Err(error) => {
            return rsx! {
                div { class: "flex flex-col gap-[22px]",
                    Crumbs { library_id: library.clone(), library: None, trail: Vec::new(), here: None }
                    Failed { error, on_retry: move |_| { header.restart(); rows.restart(); } }
                }
            };
        }
    };
    let now = now();
    let kinds = selectable(lib.kind);
    let submit_search = {
        let go = go.clone();
        let view = view.clone();
        move |e: FormEvent| {
            e.prevent_default();
            go(View {
                page: 1,
                text: search(),
                ..view.clone()
            });
        }
    };
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            Crumbs { library_id: library.clone(), library: None, trail: Vec::new(), here: Some(lib.name.clone()) }
            div { class: "cat-head",
                span { class: "cat-head-icon", KindIcon { kind: lib.kind } }
                div { class: "flex min-w-0 flex-grow flex-col gap-[3px]",
                    h1 { class: "page-title page-title-admin", "{lib.name}" }
                    span { class: "cat-source",
                        span { class: "plex-dot" }
                        "{header_line(&lib, now)}"
                    }
                }
                if lib.kind != LibraryKind::Music {
                    super::optimized::OptimizeButton {
                        kind: spoolframe_admin_api::OptimizedScopeKind::Library,
                        id: lib.id.clone(),
                        name: lib.name.clone(),
                    }
                }
                span { class: "cat-readonly", icons::Eye { size: 13 } "Read-only · fix matches in Plex" }
            }
            div { class: "cat-tools",
                form { class: "cat-search", role: "search", onsubmit: submit_search,
                    icons::Search { size: 16 }
                    input {
                        r#type: "search",
                        aria_label: "Search this library",
                        placeholder: "Search this library",
                        value: "{search}",
                        oninput: move |e| search.set(e.value()),
                    }
                }
                div { class: "cat-chips",
                    for f in FILTERS {
                        {
                            let on = view.filter == Some(f);
                            let go = go.clone();
                            let view = view.clone();
                            rsx! {
                                button {
                                    key: "{filter_name(f)}",
                                    class: if on { "cat-chip cat-chip-on" } else { "cat-chip" },
                                    aria_pressed: "{on}",
                                    onclick: move |_| go(View { page: 1, filter: if on { None } else { Some(f) }, ..view.clone() }),
                                    "{filter_label(f)}"
                                    if let Some(n) = filter_count(&lib, f) {
                                        span { class: "cat-chip-count", "{thousands(n)}" }
                                    }
                                }
                            }
                        }
                    }
                }
                if !kinds.is_empty() {
                    select {
                        class: "cat-select",
                        aria_label: "Kind",
                        onchange: {
                            let go = go.clone();
                            let view = view.clone();
                            move |e: FormEvent| go(View { page: 1, kind: crate::catalog::kind_from_name(&e.value()), ..view.clone() })
                        },
                        option { value: "", selected: view.kind.is_none(), "Any kind" }
                        for k in kinds.iter().copied() {
                            option { key: "{kind_name(k)}", value: "{kind_name(k)}", selected: view.kind == Some(k), "{kind_plural(k)}" }
                        }
                    }
                }
                span { class: "flex-grow" }
                div { class: "relative hidden md:block",
                    button {
                        class: "btn",
                        aria_haspopup: "menu",
                        aria_expanded: "{menu()}",
                        onclick: move |_| menu.toggle(),
                        icons::Sliders { size: 14 }
                        "Columns"
                    }
                    if menu() {
                        div { class: "cat-menu", role: "menu", aria_label: "Columns",
                            div { class: "cat-menu-note", "Always shown: poster, title, year, kind, match, mapped path, file" }
                            for (label, on, flip) in [
                                ("MediaId", columns().media_id, 0),
                                ("Plex path", columns().plex_path, 1),
                                ("Added", columns().added, 2),
                                ("Updated", columns().updated, 3),
                            ] {
                                button {
                                    key: "{label}",
                                    class: "cat-menu-item",
                                    role: "menuitemcheckbox",
                                    aria_checked: "{on}",
                                    onclick: move |_| {
                                        let mut c = columns();
                                        match flip {
                                            0 => c.media_id = !c.media_id,
                                            1 => c.plex_path = !c.plex_path,
                                            2 => c.added = !c.added,
                                            _ => c.updated = !c.updated,
                                        }
                                        keep_columns(c);
                                        columns.set(c);
                                    },
                                    span { class: if on { "cat-box cat-box-on" } else { "cat-box" }, icons::Check { size: 12 } }
                                    "{label}"
                                }
                            }
                        }
                    }
                }
            }
            if let Some(f) = view.filter {
                div { class: "cat-banner",
                    "{filter_banner(f, &lib.paths, now)}"
                    if f == CatalogFilter::Missing {
                        " "
                        Link { to: Route::Server {}, "Open Server › Media paths" }
                    }
                }
            }
            match current_rows {
                None => rsx! { Skeleton {} },
                Some(Err(error)) => rsx! {
                    Failed { error, on_retry: move |_| rows.restart() }
                },
                Some(Ok(page)) => {
                    let total = page.total;
                    let empty = page.rows.is_empty();
                    let waiting = view.filter == Some(CatalogFilter::Missing)
                        && !matches!(page.paths, PathsChecked::Checked { .. } | PathsChecked::Failed { .. });
                    rsx! {
                        if waiting {
                            Skeleton { rows: 3 }
                        } else if empty {
                            Nothing {
                                text: view.text.clone(),
                                filter: view.filter,
                                kind: lib.kind,
                                on_clear: {
                                    let go = go.clone();
                                    let view = view.clone();
                                    move |_| go(View { page: 1, text: String::new(), ..view.clone() })
                                },
                                on_unfilter: {
                                    let go = go.clone();
                                    let view = view.clone();
                                    move |_| go(View { page: 1, filter: None, ..view.clone() })
                                },
                            }
                        } else {
                            section { class: "cat-table-card hidden md:block",
                                table { class: "cat-table",
                                    thead {
                                        tr {
                                            th { class: "cat-open-col" }
                                            th { "Title" }
                                            th { "Year" }
                                            th { "Kind" }
                                            th { "Match" }
                                            if shown_columns.media_id { th { "MediaId" } }
                                            if shown_columns.plex_path { th { "Plex path" } }
                                            th { "Mapped path" }
                                            th { "File" }
                                            if shown_columns.added { th { "Added" } }
                                            if shown_columns.updated { th { "Updated" } }
                                        }
                                    }
                                    tbody {
                                        for row in page.rows.iter().cloned() {
                                            Rows {
                                                key: "{row.item.id()}",
                                                library: library.clone(),
                                                row,
                                                depth: 0,
                                                columns: shown_columns,
                                                opened,
                                                // Under a filter or search, rows stand alone.
                                                tree: view.filter.is_none() && view.text.trim().is_empty(),
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "flex flex-col gap-2.5 md:hidden",
                                for row in page.rows.iter().cloned() {
                                    PhoneRow { key: "{row.item.id()}", library: library.clone(), row }
                                }
                            }
                            Pager {
                                view: view.clone(),
                                total,
                                shown: page.rows.len(),
                                all: top_count(&lib),
                                unit: unit(lib.kind),
                                on_go: {
                                    let go = go.clone();
                                    move |v: View| go(v)
                                },
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The trail over a catalog page: Libraries › the library › … › here.
#[component]
pub(super) fn Crumbs(
    library_id: String,
    library: Option<String>,
    trail: Vec<(String, String)>,
    here: Option<String>,
) -> Element {
    rsx! {
        nav { class: "cat-crumbs", aria_label: "Breadcrumb",
            Link { to: Route::Libraries {}, "Libraries" }
            if let Some(name) = library {
                span { class: "cat-crumb-sep", icons::ChevronRight { size: 14 } }
                Link { to: route_to(&library_id, &View::parse("", "", "", "", "")), "{name}" }
            }
            for (id, title) in trail {
                span { key: "s-{id}", class: "cat-crumb-sep", icons::ChevronRight { size: 14 } }
                Link {
                    to: Route::CatalogItem { library: library_id.clone(), id: id.clone(), tab: String::new() },
                    "{title}"
                }
            }
            if let Some(here) = here {
                span { class: "cat-crumb-sep", icons::ChevronRight { size: 14 } }
                span { class: "cat-crumb-here", aria_current: "page", "{here}" }
            }
        }
    }
}

/// A poster, a still, or the board's dashed "no artwork" box.
#[component]
pub(super) fn Thumb(item: Item, #[props(default)] class: String) -> Element {
    let landscape = matches!(item, Item::Episode(_));
    let shape = if landscape {
        "cat-thumb cat-thumb-wide"
    } else {
        "cat-thumb"
    };
    match thumb_of(&item) {
        Some(id) => rsx! {
            img { class: "{shape} {class}", src: artwork_src(&id, "thumb"), alt: "", loading: "lazy" }
        },
        None => rsx! {
            span { class: "{shape} cat-thumb-none {class}", title: "No artwork", icons::Film { size: 12 } }
        },
    }
}

/// A row, and when opened, its children under it.
#[component]
fn Rows(
    library: String,
    row: CatalogRow,
    depth: u32,
    columns: Columns,
    opened: Signal<HashSet<String>>,
    tree: bool,
) -> Element {
    let id = row.item.id().to_string();
    let can_open = tree && opens(&row.item);
    let open = can_open && opened.read().contains(&id);
    let children = use_resource(use_reactive(
        (&library, &id, &open),
        |(library, id, open)| async move {
            if !open {
                return Ok::<_, ApiError>(Vec::new());
            }
            let client = rpc::client().await?;
            client
                .catalog_children(library, id)
                .await
                .map_err(rpc::error)
        },
    ));
    let title = row_title(&row.item);
    let sub = subtitle(&row);
    let cell = file_cell(&row);
    let file = first_file(&row).cloned();
    let agents = agent_names(&row.agents);
    let kind = item_kind(&row.item);
    let indent = depth * 28;
    let toggle = {
        let id = id.clone();
        move |_| {
            let mut set = opened.write();
            if !set.remove(&id) {
                set.insert(id.clone());
            }
        }
    };
    let tone = match depth {
        0 => "",
        1 => "cat-row-1",
        _ => "cat-row-2",
    };
    rsx! {
        tr { class: "{tone}",
            td { class: "cat-open-col",
                if can_open {
                    button {
                        class: "cat-open",
                        aria_label: if open { format!("Close {title}") } else { format!("Open {title}") },
                        aria_expanded: "{open}",
                        style: "margin-left: {indent}px",
                        onclick: toggle,
                        span { class: if open { "cat-chevron cat-chevron-open" } else { "cat-chevron" }, icons::ChevronRight { size: 14 } }
                    }
                }
            }
            td {
                div { class: "cat-title-cell", style: if !can_open && depth > 0 { format!("padding-left: {indent}px") } else { String::new() },
                    Thumb { item: row.item.clone() }
                    span { class: "flex min-w-0 flex-col",
                        Link {
                            class: "cat-title",
                            to: Route::CatalogItem { library: library.clone(), id: id.clone(), tab: String::new() },
                            "{title}"
                        }
                        if !sub.is_empty() {
                            span { class: "cat-sub", "{sub}" }
                        }
                    }
                }
            }
            td { class: "quiet", {year(&row.item).map(|y| y.to_string()).unwrap_or_default()} }
            td { class: "quiet", "{kind_label(kind)}" }
            td {
                if row.matched {
                    span { class: "cat-agents",
                        for a in agents.iter() {
                            span { key: "{a}", class: "cat-agent", "{a}" }
                        }
                        if agents.is_empty() {
                            span { class: "cat-agent", "plex" }
                        }
                    }
                } else {
                    span { class: "state state-warn", "Unmatched" }
                }
            }
            if columns.media_id {
                td { span { class: "cat-mono", "{id}" } }
            }
            if columns.plex_path {
                td { span { class: "cat-mono cat-path-cell", title: file.as_ref().map(|f| f.plex_path.clone()).unwrap_or_default(), {file.as_ref().map(|f| f.plex_path.clone()).unwrap_or_else(|| "—".into())} } }
            }
            td {
                match &file {
                    Some(f) => rsx! {
                        span { class: if f.found { "cat-mono cat-path-cell" } else { "cat-mono cat-path-cell cat-bad" }, title: match &f.problem { Some(p) => format!("{}: {p}", f.mapped_path), None => f.mapped_path.clone() }, "{f.mapped_path}" }
                    },
                    None => rsx! { span { class: "cat-quiet", "—" } },
                }
            }
            td {
                span {
                    class: match cell {
                        FileCell::Found | FileCell::AllFound => "state state-ok",
                        FileCell::Missing | FileCell::SomeMissing(_) => "state state-bad",
                        FileCell::Unknown => "cat-quiet",
                    },
                    title: if cell == FileCell::Unknown && !opens(&row.item) { "Plex lists no file" } else if cell == FileCell::Unknown { "Not counted until the path check is done" } else { "" },
                    "{file_text(&cell)}"
                }
            }
            if columns.added {
                td { class: "quiet", {row.added_at.map(date).unwrap_or_default()} }
            }
            if columns.updated {
                td { class: "quiet", {row.updated_at.map(date).unwrap_or_default()} }
            }
        }
        if open {
            match &*children.read() {
                None => rsx! {
                    tr { class: "cat-row-1", td {} td { colspan: 9, span { class: "cat-quiet", "Loading…" } } }
                },
                Some(Err(e)) => rsx! {
                    tr { class: "cat-row-1", td {} td { colspan: 9, span { class: "state state-bad", "{e.explain()}" } } }
                },
                Some(Ok(list)) => rsx! {
                    for child in list.iter().cloned() {
                        Rows {
                            key: "{child.item.id()}",
                            library: library.clone(),
                            row: child,
                            depth: depth + 1,
                            columns,
                            opened,
                            tree,
                        }
                    }
                },
            }
        }
    }
}

/// A row as a phone shows it: a card that opens the item.
#[component]
fn PhoneRow(library: String, row: CatalogRow) -> Element {
    let id = row.item.id().to_string();
    let cell = file_cell(&row);
    let agents = agent_names(&row.agents);
    let mut line = Vec::new();
    if let Some(y) = year(&row.item) {
        line.push(y.to_string());
    }
    line.push(kind_label(item_kind(&row.item)).to_owned());
    let sub = subtitle(&row);
    if !sub.is_empty() && !matches!(row.item, Item::Show(_)) {
        line.push(sub);
    }
    line.push(if row.matched {
        if agents.is_empty() {
            "plex".into()
        } else {
            agents.join(" ")
        }
    } else {
        "Unmatched".into()
    });
    rsx! {
        Link {
            class: "cat-card",
            to: Route::CatalogItem { library, id, tab: String::new() },
            Thumb { item: row.item.clone() }
            span { class: "flex min-w-0 flex-grow flex-col gap-[2px]",
                b { class: "truncate text-sm", "{row_title(&row.item)}" }
                span { class: "cat-sub truncate", "{line.join(\" · \")}" }
                span {
                    class: match cell {
                        FileCell::Found | FileCell::AllFound => "state state-ok",
                        FileCell::Missing | FileCell::SomeMissing(_) => "state state-bad",
                        FileCell::Unknown => "cat-quiet text-xs",
                    },
                    "{file_text(&cell)}"
                }
            }
            span { class: "text-ctp-subtext0", icons::ChevronRight { size: 16 } }
        }
    }
}

/// The board's "Nothing matches": what to undo.
#[component]
fn Nothing(
    text: String,
    filter: Option<CatalogFilter>,
    kind: LibraryKind,
    on_clear: EventHandler<()>,
    on_unfilter: EventHandler<()>,
) -> Element {
    let searched = !text.trim().is_empty();
    rsx! {
        section { class: "card empty",
            icons::Search { size: 26 }
            b { "{nothing_text(&text, filter)}" }
            if searched {
                span { "{searched_text(kind)}" }
            }
            div { class: "flex flex-wrap justify-center gap-2",
                if searched {
                    button { class: "btn", onclick: move |_| on_clear.call(()), "Clear search" }
                }
                if let Some(f) = filter {
                    button { class: "btn", onclick: move |_| on_unfilter.call(()), "Turn off {filter_label(f)}" }
                }
            }
        }
    }
}

/// Numbered pages and the page size, or on a phone, back and next.
#[component]
fn Pager(
    view: View,
    total: u32,
    shown: usize,
    all: u32,
    unit: &'static str,
    on_go: EventHandler<View>,
) -> Element {
    let pages = view.pages(total);
    let at = view.page.min(pages);
    let to = {
        let view = view.clone();
        move |page: u32| View {
            page,
            ..view.clone()
        }
    };
    rsx! {
        div { class: "cat-pager",
            span { class: "cat-pager-text", "{pager_text(&view, shown, total, all, unit)}" }
            div { class: "hidden items-center gap-1 md:flex",
                button {
                    class: "cat-page",
                    aria_label: "Previous page",
                    disabled: at <= 1,
                    onclick: {
                        let to = to.clone();
                        move |_| on_go.call(to(at - 1))
                    },
                    icons::ChevronLeft { size: 16 }
                }
                for p in pager(at, pages) {
                    match p {
                        Some(n) => rsx! {
                            button {
                                key: "{n}",
                                class: if n == at { "cat-page cat-page-here" } else { "cat-page" },
                                aria_current: if n == at { "page" } else { "false" },
                                onclick: {
                                    let to = to.clone();
                                    move |_| on_go.call(to(n))
                                },
                                "{n}"
                            }
                        },
                        None => rsx! { span { class: "cat-page-gap", "…" } },
                    }
                }
                button {
                    class: "cat-page",
                    aria_label: "Next page",
                    disabled: at >= pages,
                    onclick: {
                        let to = to.clone();
                        move |_| on_go.call(to(at + 1))
                    },
                    icons::ChevronRight { size: 16 }
                }
            }
            div { class: "flex items-center gap-2 md:hidden",
                button {
                    class: "btn",
                    aria_label: "Previous page",
                    disabled: at <= 1,
                    onclick: {
                        let to = to.clone();
                        move |_| on_go.call(to(at - 1))
                    },
                    icons::ChevronLeft { size: 16 }
                }
                button {
                    class: "btn",
                    aria_label: "Next page",
                    disabled: at >= pages,
                    onclick: {
                        let to = to.clone();
                        move |_| on_go.call(to(at + 1))
                    },
                    icons::ChevronRight { size: 16 }
                }
            }
            label { class: "cat-per hidden md:flex",
                "Per page"
                select {
                    class: "cat-select",
                    onchange: {
                        let view = view.clone();
                        move |e: FormEvent| {
                            let per = e.value().parse().unwrap_or(PER_PAGE[0]);
                            on_go.call(View { page: 1, per, ..view.clone() });
                        }
                    },
                    for n in PER_PAGE {
                        option { key: "{n}", value: "{n}", selected: n == view.per, "{n}" }
                    }
                }
            }
        }
    }
}
