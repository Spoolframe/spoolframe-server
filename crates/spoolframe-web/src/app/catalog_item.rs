//! An item of a library (P3-16), as the WebCatalogItem,
//! WebCatalogFiles, WebCatalogItemTabs and WebCatalogStates boards draw
//! it: what clients get, its external IDs, its files and the paths they
//! map to, its artwork, markers and children. Read-only, but for the
//! caller's own playlists and hearts (PX-40, PX-43).

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, ArtworkInfo, ArtworkKind, CatalogItem as ItemView, CatalogKind, FileState,
    Item, Marker, MarkerKind, PathsChecked,
};

use super::catalog::{Crumbs, Forbidden, Skeleton, Thumb, Unreachable, artwork_src};
use super::{Alert, Route, icons, rpc};
use crate::catalog::{
    FileCell, bytes, clock, duration_ms, external_id, file_cell, file_text, files_chip,
    item_context, item_kind, kind_label, marker_line, marker_name, marker_span, row_title,
    running_time, tabs, uncovered, version_line,
};
use crate::logic::{ApiError, thousands};

async fn load_item(id: String) -> Result<ItemView, ApiError> {
    let client = rpc::client().await?;
    client.catalog_item(id).await.map_err(rpc::error)
}

/// The item clients get, its children and markers, from the reply.
struct Reply {
    item: Item,
    children: Vec<Item>,
    markers: Vec<Marker>,
}

fn reply(seen: &ItemView) -> Option<Reply> {
    let get = |key: &str| seen.reply.get(key).cloned();
    Some(Reply {
        item: serde_json::from_value(get("item")?).ok()?,
        children: get("children")
            .and_then(|c| serde_json::from_value(c).ok())
            .unwrap_or_default(),
        markers: get("markers")
            .and_then(|m| serde_json::from_value(m).ok())
            .unwrap_or_default(),
    })
}

#[component]
pub fn CatalogItem(library: String, id: String, tab: String) -> Element {
    let mut loaded = use_resource(use_reactive((&id,), |(id,)| load_item(id)));
    let current = loaded.read().clone();
    let tab = if tab.is_empty() {
        "overview".to_owned()
    } else {
        tab
    };
    match current {
        None => rsx! {
            div { class: "flex flex-col gap-[22px]",
                Crumbs { library_id: library.clone(), library: Some(String::new()), trail: Vec::new(), here: None }
                Skeleton { rows: 4 }
            }
        },
        Some(Err(error)) => rsx! {
            div { class: "flex flex-col gap-[22px]",
                Crumbs { library_id: library.clone(), library: None, trail: Vec::new(), here: None }
                match error.code {
                    spoolframe_admin_api::error_code::FORBIDDEN => rsx! { Forbidden {} },
                    spoolframe_admin_api::error_code::UNAVAILABLE => rsx! {
                        Unreachable { error, on_retry: move |_| loaded.restart() }
                    },
                    _ => rsx! { Alert { text: error.explain() } },
                }
            }
        },
        Some(Ok(seen)) => match reply(&seen) {
            None => {
                rsx! { Alert { text: "The server's answer for this item could not be read.".to_owned() } }
            }
            Some(reply) => rsx! {
                Page { library, seen, reply_item: reply.item, client_children: reply.children, markers: reply.markers, tab, on_reload: move |_| loaded.restart() }
            },
        },
    }
}

#[component]
fn Page(
    library: String,
    seen: ItemView,
    reply_item: Item,
    client_children: Vec<Item>,
    markers: Vec<Marker>,
    tab: String,
    /// Read the item again: its heart changed.
    on_reload: EventHandler<()>,
) -> Element {
    let item = reply_item;
    let heart = crate::playlists::Heart::of_reply(&seen.reply);
    let id = item.id().to_string();
    let kind = item_kind(&item);
    let title = match &item {
        Item::Episode(e) => e.title.clone(),
        other => row_title(other),
    };
    let trail: Vec<(String, String)> = seen
        .ancestors
        .iter()
        .map(|c| (c.id.clone(), c.title.clone()))
        .collect();
    let library_name = seen.library.as_ref().map(|l| l.title.clone());
    let files = files_chip(&seen.versions);
    let mut copied = use_signal(|| false);
    let copy = {
        let id = id.clone();
        move |_| {
            if let Some(window) = web_sys::window() {
                let _ = window.navigator().clipboard().write_text(&id);
                copied.set(true);
            }
        }
    };
    let has_markers = matches!(kind, CatalogKind::Movie | CatalogKind::Episode);
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            Crumbs { library_id: library.clone(), library: library_name, trail, here: Some(title.clone()) }
            div { class: "cat-item-head",
                Thumb { item: item.clone(), class: "cat-thumb-head" }
                div { class: "flex min-w-0 flex-grow flex-col gap-1.5",
                    h1 { class: "page-title page-title-admin", "{title}" }
                    span { class: "cat-context", "{item_context(&item)}" }
                    div { class: "cat-chips-line",
                        span { class: "cat-id-box",
                            span { class: "cat-id-label", "MediaId" }
                            span { class: "cat-mono", "{id}" }
                            button {
                                class: "cat-copy",
                                aria_label: "Copy the MediaId",
                                title: if copied() { "Copied" } else { "Copy" },
                                onclick: copy,
                                if copied() { icons::Check { size: 13 } } else { icons::LinkChain { size: 13 } }
                            }
                        }
                        if seen.matched {
                            span { class: "state state-ok cat-chip-state", icons::CircleCheck { size: 14 } "Matched" }
                        } else {
                            span { class: "state state-warn cat-chip-state", icons::Warning { size: 14 } "Unmatched" }
                        }
                        if let Some((ok, text)) = &files {
                            span {
                                class: if *ok { "state state-ok cat-chip-state" } else { "state state-bad cat-chip-state" },
                                if *ok { icons::CircleCheck { size: 14 } } else { icons::CircleX { size: 14 } }
                                "{text}"
                            }
                        }
                        if has_markers {
                            span {
                                class: if markers.is_empty() { "cat-quiet cat-chip-state" } else { "state state-ok cat-chip-state" },
                                if !markers.is_empty() { icons::CircleCheck { size: 14 } }
                                {match markers.len() { 0 => "No markers".to_owned(), 1 => "1 marker".to_owned(), n => format!("{n} markers") }}
                            }
                        }
                    }
                }
                if let Some(heart) = heart {
                    super::playlists::HeartButton { id: id.clone(), heart, kind, title: title.clone(), word: true, on_changed: on_reload }
                }
                super::playlists::AddToPlaylist { item: item.clone() }
                if let Some(link) = &seen.open_in_plex {
                    a { class: "btn shrink-0", href: "{link}", target: "_blank", rel: "noopener noreferrer",
                        icons::ExternalLink { size: 14 }
                        "Open in Plex"
                    }
                }
            }
            nav { class: "cat-tabs", role: "tablist",
                for (key, label) in tabs(kind) {
                    Link {
                        key: "{key}",
                        class: if key == tab { "cat-tab cat-tab-here" } else { "cat-tab" },
                        role: "tab",
                        aria_selected: "{key == tab}",
                        to: Route::CatalogItem { library: library.clone(), id: id.clone(), tab: if key == "overview" { String::new() } else { key.to_owned() } },
                        "{label}"
                        if key == "debug" {
                            icons::Activity { size: 14 }
                        }
                    }
                }
            }
            match tab.as_str() {
                "files" => rsx! { Files { seen: seen.clone() } },
                "artwork" => rsx! { ArtworkTab { id: id.clone(), server: seen.server_name.clone() } },
                "markers" => rsx! { Markers { markers: markers.clone(), duration: duration_ms(&item), server: seen.server_name.clone() } },
                "children" => rsx! { Children { library: library.clone(), seen: seen.clone(), client_children: client_children.clone(), kind } },
                "debug" => rsx! { super::catalog_debug::DebugTab { id: id.clone() } },
                _ => rsx! { Overview { seen: seen.clone(), item: item.clone(), library: library.clone() } },
            }
        }
    }
}

/// A field of "As clients get it".
#[component]
fn Field(label: String, children: Element) -> Element {
    rsx! {
        div { class: "cat-field",
            span { class: "cat-field-label", "{label}" }
            span { class: "cat-field-value", {children} }
        }
    }
}

/// An id clients know, linked to its page here.
#[component]
fn IdLink(library: String, title: String, id: String) -> Element {
    rsx! {
        Link { to: Route::CatalogItem { library, id: id.clone(), tab: String::new() }, "{title}" }
        " "
        span { class: "cat-mono cat-quiet", "{id}" }
    }
}

#[component]
fn Overview(seen: ItemView, item: Item, library: String) -> Element {
    let mut json = use_signal(|| false);
    let kind = item_kind(&item);
    let crumb = |id: &str| {
        seen.ancestors
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.title.clone())
            .unwrap_or_else(|| id.to_owned())
    };
    let artwork = match &item {
        Item::Movie(v) => v.artwork.clone(),
        Item::Show(v) => v.artwork.clone(),
        Item::Season(v) => v.artwork.clone(),
        Item::Episode(v) => v.artwork.clone(),
        Item::Artist(v) => v.artwork.clone(),
        Item::Album(v) => v.artwork.clone(),
        Item::Track(_) => Vec::new(),
    };
    let summary = match &item {
        Item::Movie(v) => v.summary.clone(),
        Item::Show(v) => v.summary.clone(),
        Item::Episode(v) => v.summary.clone(),
        Item::Artist(v) => v.summary.clone(),
        _ => None,
    };
    let pretty = serde_json::to_string_pretty(&seen.reply).unwrap_or_default();
    let ids: Vec<_> = seen
        .external_ids
        .iter()
        .filter(|i| !i.starts_with("plex://"))
        .map(|i| external_id(i, kind))
        .collect();
    rsx! {
        div { class: "cat-overview",
            section { class: "card",
                div { class: "card-head",
                    h2 { "As clients get it" }
                    span { class: "cat-quiet text-xs", "{kind_label(kind)}, from the catalog" }
                }
                div { class: "cat-fields",
                    Field { label: "Title", {row_title_plain(&item)} }
                    match &item {
                        Item::Season(s) => rsx! {
                            Field { label: "Show", IdLink { library: library.clone(), title: crumb(&s.show_id.to_string()), id: s.show_id.to_string() } }
                            Field { label: "Season", "{s.season}" }
                        },
                        Item::Episode(e) => rsx! {
                            Field { label: "Show", IdLink { library: library.clone(), title: crumb(&e.show_id.to_string()), id: e.show_id.to_string() } }
                            Field { label: "Season", IdLink { library: library.clone(), title: crumb(&e.season_id.to_string()), id: e.season_id.to_string() } }
                            Field { label: "Episode", "{e.episode}" }
                            if let Some(date) = e.air_date { Field { label: "Aired", "{date}" } }
                        },
                        Item::Album(a) => rsx! {
                            Field { label: "Artist", IdLink { library: library.clone(), title: crumb(&a.artist_id.to_string()), id: a.artist_id.to_string() } }
                        },
                        Item::Track(t) => rsx! {
                            Field { label: "Album", IdLink { library: library.clone(), title: crumb(&t.album_id.to_string()), id: t.album_id.to_string() } }
                            Field { label: "Artist", IdLink { library: library.clone(), title: crumb(&t.artist_id.to_string()), id: t.artist_id.to_string() } }
                            if let Some(n) = t.track { Field { label: "Track", "{n}" } }
                            if let Some(n) = t.disc { Field { label: "Disc", "{n}" } }
                        },
                        _ => rsx! {},
                    }
                    if let Some(year) = crate::catalog::year(&item) {
                        Field { label: "Year", "{year}" }
                    }
                    if let Some(ms) = duration_ms(&item) {
                        Field { label: "Duration", "{running_time(ms)} " span { class: "cat-mono cat-quiet", "{thousands(ms)} ms" } }
                    }
                    if let Some(text) = summary {
                        Field { label: "Summary", "{text}" }
                    }
                    if !artwork.is_empty() {
                        Field { label: "Artwork",
                            span { class: "flex flex-col gap-1",
                                for a in artwork.iter() {
                                    span { key: "{a.id}",
                                        "{artwork_kind(a.kind)} "
                                        span { class: "cat-mono cat-quiet", "{a.id}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "flex flex-col gap-4",
                // Its subtitles from OpenSubtitles and SubDL (PX-02): a film's or an
                // episode's, the items with a file.
                if matches!(kind, CatalogKind::Movie | CatalogKind::Episode) {
                    super::subtitle_search::SubtitlesCard { id: item.id().to_string() }
                }
                // The playlists holding it (PX-40), as clients list them;
                // a show's are among its seasons instead.
                if !crate::playlists::held_as_seasons(kind) {
                    super::playlists::HeldInCard { held: crate::playlists::held_in(&seen.reply) }
                }
                section { class: "card",
                    div { class: "card-head",
                        h2 { "External IDs" }
                        span { class: "cat-quiet text-xs", "P1-14" }
                    }
                    div { class: "cat-fields",
                        if ids.is_empty() && seen.guid.is_none() {
                            p { class: "m-0 text-sm text-ctp-subtext0", "Plex has none for it: it is not matched." }
                        }
                        for x in ids {
                            Field { key: "{x.label}{x.value}", label: x.label.clone(),
                                span { class: "cat-mono", "{x.value}" }
                                if let (Some(url), Some(site)) = (&x.url, x.site) {
                                    " "
                                    a { href: "{url}", target: "_blank", rel: "noopener noreferrer", "{site} ↗" }
                                }
                            }
                        }
                        if let Some(guid) = &seen.guid {
                            Field { label: "Plex GUID", span { class: "cat-mono cat-break", "{guid}" } }
                        }
                    }
                }
                section { class: "card",
                    div { class: "card-head", h2 { "Raw" } }
                    div { class: "card-body",
                        p { class: "m-0 text-sm text-ctp-subtext0", "The exact value a client receives, as JSON." }
                        div {
                            button { class: "btn", aria_expanded: "{json()}", onclick: move |_| json.toggle(),
                                if json() { icons::ChevronUp { size: 14 } } else { icons::ChevronDown { size: 14 } }
                                if json() { "Hide JSON" } else { "Show JSON" }
                            }
                        }
                        if json() {
                            pre { class: "cat-json", "{pretty}" }
                        }
                    }
                }
            }
        }
    }
}

fn row_title_plain(item: &Item) -> String {
    match item {
        Item::Episode(e) => e.title.clone(),
        other => row_title(other),
    }
}

fn artwork_kind(kind: ArtworkKind) -> &'static str {
    match kind {
        ArtworkKind::Poster => "Poster",
        ArtworkKind::Background => "Background",
        ArtworkKind::Thumb => "Thumb",
    }
}

/// Where to change the mappings of `server`: Settings › Paths for the
/// first Plex server, Server › Libraries' "Path mapping" for the others.
fn mappings_route(server: &str) -> Route {
    if server == "plex" {
        Route::Settings {
            tab: "paths".into(),
        }
    } else {
        Route::Libraries {}
    }
}

#[component]
fn PathRows(file: FileState, server: String, server_name: String) -> Element {
    rsx! {
        div { class: "cat-path-grid",
            span { class: "cat-field-label", "Plex path" }
            span { class: "cat-mono cat-break", "{file.plex_path}" }
            span { class: "cat-field-label", "Mapping" }
            match &file.mapping {
                Some(m) => rsx! {
                    span {
                        span { class: "cat-mono", "{m.from} → {m.to}" }
                        span { class: "cat-quiet text-xs",
                            " · {server_name}"
                            if let Some(l) = &m.library { " · {l} only" }
                            " "
                        }
                        Link { class: "text-xs", to: mappings_route(&server), "Edit mappings" }
                    }
                },
                None if file.found => rsx! {
                    span { class: "cat-quiet", "None needed: Plex's path is read as it is." }
                },
                None => rsx! {
                    span { class: "cat-bad",
                        "No mapping covers "
                        span { class: "cat-mono", "{uncovered(&file.plex_path)}" }
                        ". "
                        Link { to: mappings_route(&server), "Add one" }
                    }
                },
            }
            span { class: "cat-field-label", "Mapped path" }
            if file.mapping.is_none() && !file.found {
                span { class: "cat-bad", "—" }
            } else {
                span { class: if file.found { "cat-mono cat-break" } else { "cat-mono cat-break cat-bad" }, title: file.problem.clone().unwrap_or_default(), "{file.mapped_path}" }
            }
        }
    }
}

#[component]
fn Files(seen: ItemView) -> Element {
    rsx! {
        div { class: "flex flex-col gap-4",
            if seen.versions.is_empty() && seen.folders.is_empty() {
                section { class: "card", div { class: "card-body",
                    p { class: "m-0 text-sm text-ctp-subtext0", "Plex lists no file for it: its files are its children's." }
                } }
            }
            for folder in seen.folders.iter() {
                section { key: "{folder}", class: "card",
                    div { class: "card-head", h2 { "Folder" } }
                    div { class: "card-body",
                        div { class: "cat-path-grid",
                            span { class: "cat-field-label", "Plex path" }
                            span { class: "cat-mono cat-break", "{folder}" }
                        }
                        p { class: "m-0 text-xs text-ctp-subtext0", "Its files are its episodes' and tracks', each on its own page." }
                    }
                }
            }
            for (i, v) in seen.versions.iter().enumerate() {
                section { key: "{i}", class: "card",
                    div { class: "card-head",
                        h2 {
                            "File {i + 1}"
                            if !version_line(v).is_empty() { " · {version_line(v)}" }
                        }
                        if let Some(id) = v.id {
                            span { class: "cat-quiet text-xs", "Plex media {id}" }
                        }
                    }
                    div { class: "card-body gap-4",
                        for (j, p) in v.parts.iter().enumerate() {
                            div { key: "{j}", class: if j > 0 { "cat-part cat-part-next" } else { "cat-part" },
                                div { class: "row",
                                    span { class: "flex items-baseline gap-2",
                                        b { "Part {j + 1}" }
                                        span { class: "cat-quiet text-[13px]",
                                            {[p.size.map(bytes), p.duration_ms.map(running_time)].into_iter().flatten().collect::<Vec<_>>().join(" · ")}
                                        }
                                        if let Some(id) = p.id {
                                            span { class: "cat-quiet text-xs", "· part {id}" }
                                        }
                                    }
                                    if p.file.found {
                                        span { class: "state state-ok cat-chip-state", icons::CircleCheck { size: 14 } "Found" }
                                    } else {
                                        span { class: "state state-bad cat-chip-state", title: p.file.problem.clone().unwrap_or_default(), icons::CircleX { size: 14 } "Not found" }
                                    }
                                }
                                PathRows { file: p.file.clone(), server: seen.server.clone(), server_name: seen.server_name.clone() }
                            }
                        }
                    }
                }
            }
            if !seen.versions.is_empty() {
                p { class: "m-0 text-[13px] text-ctp-subtext0",
                    "Checked when the page opened. The library-wide check runs from "
                    Link { to: Route::Server {}, "Server › Media paths" }
                    "."
                    if seen.versions.len() > 1 { " Playback reads File 1." }
                }
            }
        }
    }
}

#[component]
fn ArtworkTab(id: String, server: String) -> Element {
    let infos = use_resource(use_reactive((&id,), |(id,)| async move {
        let client = rpc::client().await?;
        client.catalog_artwork(id).await.map_err(rpc::error)
    }));
    let current = infos.read().clone();
    match current {
        None => rsx! { Skeleton { rows: 2 } },
        Some(Err(e)) => rsx! { Alert { text: e.explain() } },
        Some(Ok(list)) => {
            let of = |kind: ArtworkKind| list.iter().find(|a| a.kind == kind).cloned();
            rsx! {
                section { class: "card",
                    div { class: "card-body cat-art-grid",
                        for kind in [ArtworkKind::Poster, ArtworkKind::Background, ArtworkKind::Thumb] {
                            ArtworkCell { key: "{artwork_kind(kind)}", kind, info: of(kind), server: server.clone() }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ArtworkCell(kind: ArtworkKind, info: Option<ArtworkInfo>, server: String) -> Element {
    let shape = match kind {
        ArtworkKind::Poster => "cat-art cat-art-poster",
        _ => "cat-art cat-art-wide",
    };
    match info {
        None => rsx! {
            div { class: "cat-art-cell",
                span { class: "{shape} cat-art-none", "None" }
                b { "{artwork_kind(kind)}" }
                span { class: "cat-quiet text-xs", "None · Clients show a placeholder" }
            }
        },
        Some(info) => {
            let mut facts = Vec::new();
            if let (Some(w), Some(h)) = (info.width, info.height) {
                facts.push(format!("{w}×{h}"));
            }
            if let Some(b) = info.bytes {
                facts.push(bytes(b));
            }
            facts.push(format!("from Plex · {server}"));
            rsx! {
                div { class: "cat-art-cell",
                    img { class: "{shape}", src: artwork_src(&info.id, "medium"), alt: "{artwork_kind(kind)}" }
                    b { "{artwork_kind(kind)}" }
                    span { class: "cat-mono cat-break text-xs", "{info.id}" }
                    if let Some(e) = &info.error {
                        span { class: "cat-bad text-xs", "{e}" }
                    } else {
                        span { class: "cat-quiet text-xs", "{facts.join(\" · \")}" }
                    }
                }
            }
        }
    }
}

#[component]
fn Markers(markers: Vec<Marker>, duration: Option<u64>, server: String) -> Element {
    let shown: Vec<Marker> = markers
        .into_iter()
        .filter(|m| m.kind != MarkerKind::Unknown)
        .collect();
    let total = duration
        .or_else(|| shown.iter().map(|m| m.end_ms).max())
        .unwrap_or(0);
    rsx! {
        section { class: "card",
            div { class: "card-body",
                if shown.is_empty() {
                    p { class: "m-0 text-sm text-ctp-subtext0",
                        "Plex found no intro or credits in it. Plex finds them with Plex Pass and its intro and credits detection on."
                    }
                } else {
                    div { class: "cat-timeline", role: "img", aria_label: "Where the markers are in the item",
                        for (i, m) in shown.iter().enumerate() {
                            {
                                let (left, width) = marker_span(m, total);
                                rsx! {
                                    span {
                                        key: "{i}",
                                        class: if m.kind == MarkerKind::Intro { "cat-mark cat-mark-intro" } else { "cat-mark cat-mark-credits" },
                                        style: "left: {left}%; width: {width}%",
                                    }
                                }
                            }
                        }
                    }
                    div { class: "flex justify-between",
                        span { class: "cat-mono cat-quiet text-xs", "0:00" }
                        span { class: "cat-mono cat-quiet text-xs", "{clock(total)}" }
                    }
                    for (i, m) in shown.iter().enumerate() {
                        div { key: "{i}", class: "cat-marker-row",
                            span { class: "flex items-center gap-2",
                                span { class: if m.kind == MarkerKind::Intro { "cat-dot cat-mark-intro" } else { "cat-dot cat-mark-credits" } }
                                "{marker_name(m.kind)}"
                            }
                            span { class: "cat-mono", "{marker_line(m)}" }
                            span { class: "cat-quiet text-xs", "from Plex · {server}" }
                        }
                    }
                    p { class: "m-0 text-[13px] text-ctp-subtext0", "Clients offer Skip intro and Skip credits from these (P2-10)." }
                }
            }
        }
    }
}

/// A playlist holding some of a show's episodes, as a row of its
/// seasons: its cover, its name and whose it is, opening the playlist.
#[component]
fn HeldRow(held: crate::playlists::HeldIn) -> Element {
    let line = crate::playlists::held_line(&held);
    let src = crate::playlists::cover_src(&held.id, held.artwork.as_deref(), "thumb");
    rsx! {
        Link { class: "cat-child", to: Route::PlaylistView { id: held.id.clone() },
            if held.artwork.is_some() {
                img { class: "cat-thumb cat-thumb-cover", src, alt: "", loading: "lazy" }
            } else {
                span { class: "cat-thumb cat-thumb-none", title: "No cover", icons::Film { size: 12 } }
            }
            span { class: "flex min-w-0 flex-grow flex-col",
                b { class: "text-sm", "{held.name}" }
                span { class: "cat-quiet text-xs", "{line}" }
            }
            span { class: "text-ctp-subtext0", icons::ChevronRight { size: 16 } }
        }
    }
}

#[component]
fn Children(
    library: String,
    seen: ItemView,
    client_children: Vec<Item>,
    kind: CatalogKind,
) -> Element {
    let rows = seen.children.clone();
    let checked = matches!(seen.paths, PathsChecked::Checked { .. });
    // An album's tracks the caller liked (PX-43).
    let liked = crate::playlists::liked_children(&seen.reply);
    // A show's playlists, after its seasons, as a season is shown.
    let held = if crate::playlists::held_as_seasons(kind) {
        crate::playlists::held_in(&seen.reply)
    } else {
        Vec::new()
    };
    rsx! {
        section { class: "card",
            div { class: "card-body gap-0 py-0",
                if rows.is_empty() && held.is_empty() {
                    p { class: "py-4 text-sm text-ctp-subtext0", "It has none." }
                }
                for row in rows.iter().cloned() {
                    {
                        let cell = file_cell(&row);
                        let id = row.item.id().to_string();
                        let count = match (&row.item, row.leaves, row.children) {
                            (Item::Track(t), _, _) => t.duration.map(|d| running_time(u64::try_from(d.as_millis()).unwrap_or(0))).unwrap_or_default(),
                            (_, Some(n), _) => crate::logic::count(u64::from(n), if matches!(row.item, Item::Album(_)) { "track" } else { "episode" }),
                            (_, None, Some(n)) => crate::logic::count(u64::from(n), "album"),
                            _ => String::new(),
                        };
                        rsx! {
                            Link {
                                key: "{id}",
                                class: "cat-child",
                                to: Route::CatalogItem { library: library.clone(), id: id.clone(), tab: String::new() },
                                Thumb { item: row.item.clone() }
                                span { class: "flex min-w-0 flex-grow flex-col",
                                    b { class: "text-sm", "{row_title(&row.item)}" }
                                    span { class: "cat-mono cat-quiet text-xs", "{id}" }
                                }
                                if liked.contains(&id) {
                                    span { class: "text-ctp-red", title: "Liked", aria_label: "Liked",
                                        icons::Heart { size: 14, full: true }
                                    }
                                }
                                span { class: "cat-child-count", "{count}" }
                                span {
                                    class: match cell {
                                        FileCell::Found | FileCell::AllFound => "state state-ok cat-child-state",
                                        FileCell::Missing | FileCell::SomeMissing(_) => "state state-bad cat-child-state",
                                        FileCell::Unknown => "cat-quiet cat-child-state text-xs",
                                    },
                                    "{file_text(&cell)}"
                                }
                                span { class: "text-ctp-subtext0", icons::ChevronRight { size: 16 } }
                            }
                        }
                    }
                }
                for p in held {
                    HeldRow { key: "{p.id}", held: p }
                }
            }
        }
        if !checked && rows.iter().any(|r| r.files.is_empty()) {
            p { class: "m-0 text-[13px] text-ctp-subtext0", "Missing files are counted once the library's path check is done." }
        }
        // The client's children, which the rows are, in the same order.
        if rows.len() != client_children.len() {
            p { class: "m-0 text-[13px] text-ctp-subtext0", "Clients get {client_children.len()} children." }
        }
    }
}
