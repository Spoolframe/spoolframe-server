//! The libraries paired servers share with this one (experimental
//! `federation`, P8-02), browsed by an admin: under Server › Libraries by
//! server, each with whether it is online; a library's items with the
//! same paging, sort and filters as this server's own; an item as far as
//! this server's cache of it goes. Read-only, and all of it from the
//! cache, so an offline pair's libraries still open, marked so.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, Item, ItemFilter, ItemFilterValues, PairedItem as ItemView, PairedPage,
    PairedQuery, PairedServer, error_code,
};

use super::catalog::{Filters, KindIcon, Pager, Skeleton, SortSelect, Thumb};
use super::{Alert, Route, icons, rpc};
use crate::browse;
use crate::catalog::{View, external_id, item_context, item_kind, row_title};
use crate::logic::ApiError;

async fn load_servers() -> Result<Vec<PairedServer>, ApiError> {
    let client = rpc::client().await?;
    client.catalog_paired_libraries().await.map_err(rpc::error)
}

async fn load_page(library: String, view: View) -> Result<PairedPage, ApiError> {
    let client = rpc::client().await?;
    client
        .catalog_paired_items(PairedQuery {
            library,
            offset: view.offset(),
            limit: view.per,
            sort: view.sort,
            item_filter: (!view.items.is_empty()).then(|| view.items.clone()),
        })
        .await
        .map_err(rpc::error)
}

async fn load_values(library: String) -> Result<ItemFilterValues, ApiError> {
    let client = rpc::client().await?;
    client
        .catalog_filter_values(library)
        .await
        .map_err(rpc::error)
}

async fn load_item(id: String) -> Result<ItemView, ApiError> {
    let client = rpc::client().await?;
    client.catalog_paired_item(id).await.map_err(rpc::error)
}

/// The route to `view` of paired library `library`.
fn route_to(library: &str, view: &View) -> Route {
    Route::PairedLibrary {
        library: library.to_owned(),
        page: if view.page > 1 {
            view.page.to_string()
        } else {
            String::new()
        },
        per: if view.per != crate::catalog::PER_PAGE[0] {
            view.per.to_string()
        } else {
            String::new()
        },
        filter: view.filter_param(),
        sort: view.sort_param(),
    }
}

/// Online, or offline and browsed from the cache: a dot and the words.
#[component]
fn Reach(online: bool) -> Element {
    rsx! {
        span { class: "cat-source",
            span { class: if online { "reach-dot reach-on" } else { "reach-dot" } }
            if online { "Online" } else { "Offline · shown as last synced" }
        }
    }
}

/// Server › Libraries' "Shared with this server": each paired server's
/// libraries under its name. Nothing at all with federation off, or
/// when no paired server shares anything.
#[component]
pub fn PairedServers() -> Element {
    let servers = use_resource(load_servers);
    let current = servers.read().clone();
    match current {
        None => rsx! {},
        Some(Err(error)) if error.code == error_code::NOT_CONFIGURED => rsx! {},
        Some(Err(error)) => rsx! {
            Alert { text: format!("Couldn't load the paired servers' libraries: {}", error.explain()) }
        },
        Some(Ok(servers)) if servers.is_empty() => rsx! {},
        Some(Ok(servers)) => rsx! {
            div { class: "flex flex-col gap-3",
                h2 { class: "m-0 text-base font-bold", "Shared with this server" }
                for server in servers {
                    section { key: "{server.server_id}", class: "lib-table",
                        div { class: "paired-head",
                            span { class: "flex items-center gap-3",
                                b { "{server.name}" }
                                Reach { online: server.online }
                            }
                        }
                        for library in server.libraries.iter().cloned() {
                            div { key: "{library.id}", class: "paired-row",
                                span { class: "lib-icon", KindIcon { kind: library.kind } }
                                span { class: "lib-what",
                                    Link {
                                        class: "lib-name",
                                        to: Route::PairedLibrary {
                                            library: library.id.clone(),
                                            page: String::new(),
                                            per: String::new(),
                                            filter: String::new(),
                                            sort: String::new(),
                                        },
                                        "{library.title}"
                                    }
                                    span { class: "lib-source", "From {server.name} · read-only" }
                                }
                            }
                        }
                    }
                }
            }
        },
    }
}

/// The trail over a paired server's pages: Libraries › server › library.
#[component]
fn Trail(server: String, library: Option<(String, String)>, here: Option<String>) -> Element {
    rsx! {
        nav { class: "cat-crumbs", aria_label: "Breadcrumb",
            Link { to: Route::Libraries {}, "Libraries" }
            span { class: "cat-crumb-sep", icons::ChevronRight { size: 14 } }
            span { "{server}" }
            if let Some((id, title)) = library {
                span { class: "cat-crumb-sep", icons::ChevronRight { size: 14 } }
                if here.is_some() {
                    Link {
                        to: Route::PairedLibrary {
                            library: id,
                            page: String::new(),
                            per: String::new(),
                            filter: String::new(),
                            sort: String::new(),
                        },
                        "{title}"
                    }
                } else {
                    span { class: "cat-crumb-here", aria_current: "page", "{title}" }
                }
            }
            if let Some(here) = here {
                span { class: "cat-crumb-sep", icons::ChevronRight { size: 14 } }
                span { class: "cat-crumb-here", aria_current: "page", "{here}" }
            }
        }
    }
}

/// A paired server's library: its items, from the cache, a page at a
/// time, sorted and filtered as clients ask it.
#[component]
pub fn PairedLibrary(
    library: String,
    page: String,
    per: String,
    filter: String,
    sort: String,
) -> Element {
    let view = View::parse_sorted(&page, &per, &filter, "", "", &sort);
    let nav = navigator();
    let mut rows = use_resource(use_reactive((&library, &view), |(library, view)| {
        load_page(library, view)
    }));
    let values = use_resource(use_reactive((&library,), |(library,)| load_values(library)));
    let mut filters_open = use_signal(|| false);
    let go = {
        let library = library.clone();
        move |next: View| {
            nav.push(route_to(&library, &next));
        }
    };
    let offered = match &*values.read() {
        Some(Ok(values)) => Some(values.clone()),
        _ => None,
    };
    let current = rows.read().clone();
    let Some(loaded) = current else {
        return rsx! { Skeleton {} };
    };
    let page = match loaded {
        Ok(page) => page,
        Err(error) => {
            return rsx! {
                div { class: "flex flex-col gap-[22px]",
                    Alert { text: error.explain() }
                    span {
                        button { class: "btn", onclick: move |_| rows.restart(), icons::RotateCcw {} "Try again" }
                    }
                }
            };
        }
    };
    let unit = crate::catalog::unit(page.library.kind);
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            Trail {
                server: page.server_name.clone(),
                library: Some((page.library.id.clone(), page.library.title.clone())),
                here: None,
            }
            div { class: "cat-head",
                span { class: "cat-head-icon", KindIcon { kind: page.library.kind } }
                div { class: "flex min-w-0 flex-grow flex-col gap-[3px]",
                    h1 { class: "page-title page-title-admin", "{page.library.title}" }
                    span { class: "flex flex-wrap items-center gap-3",
                        span { class: "cat-quiet", "Shared by {page.server_name}" }
                        Reach { online: page.online }
                    }
                }
                span { class: "cat-readonly", icons::Eye { size: 13 } "Read-only · it is {page.server_name}'s" }
            }
            if let Some(offered) = offered {
                div { class: "cat-tools",
                    if !offered.sorts.is_empty() {
                        SortSelect {
                            view: view.clone(),
                            offered: offered.clone(),
                            on_go: {
                                let go = go.clone();
                                move |v: View| go(v)
                            },
                        }
                    }
                    if !offered.filters.is_empty() {
                        div { class: "relative",
                            button {
                                class: if view.items.is_empty() { "btn" } else { "btn cat-chip-on" },
                                aria_haspopup: "dialog",
                                aria_expanded: "{filters_open()}",
                                onclick: move |_| filters_open.toggle(),
                                icons::Sliders { size: 14 }
                                "Filters"
                            }
                            if filters_open() {
                                Filters {
                                    view: view.clone(),
                                    offered,
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
            if !view.items.is_empty() {
                div { class: "cat-banner flex flex-wrap items-center gap-2",
                    "Only {browse::filter_text(&view.items)}"
                    button {
                        class: "cat-chip",
                        onclick: {
                            let go = go.clone();
                            let view = view.clone();
                            move |_| go(view.browsing(view.sort, ItemFilter::default()))
                        },
                        icons::X { size: 12 }
                        "Clear filters"
                    }
                }
            }
            if page.items.is_empty() {
                section { class: "card empty",
                    if view.items.is_empty() {
                        b { "Nothing is cached of this library yet" }
                        span { "It fills in at the next sync with {page.server_name}." }
                    } else {
                        b { "Nothing here with these filters" }
                    }
                }
            } else {
                section { class: "cat-table-card",
                    table { class: "cat-table",
                        thead {
                            tr {
                                th { "Title" }
                                th { "Year" }
                                th { "Kind" }
                            }
                        }
                        tbody {
                            for item in page.items.iter().cloned() {
                                PairedRow { key: "{item.id()}", library: library.clone(), item }
                            }
                        }
                    }
                }
                Pager {
                    view: view.clone(),
                    total: page.total,
                    shown: page.items.len(),
                    all: page.total,
                    unit,
                    on_go: {
                        let go = go.clone();
                        move |v: View| go(v)
                    },
                }
            }
        }
    }
}

#[component]
fn PairedRow(library: String, item: Item) -> Element {
    let id = item.id().to_string();
    let title = row_title(&item);
    let year = crate::catalog::year(&item)
        .map(|y| y.to_string())
        .unwrap_or_default();
    let kind = crate::catalog::kind_label(item_kind(&item));
    rsx! {
        tr {
            td {
                Link {
                    class: "flex items-center gap-3",
                    to: Route::PairedItem { library: library.clone(), id },
                    Thumb { item: item.clone() }
                    b { "{title}" }
                }
            }
            td { class: "quiet", "{year}" }
            td { class: "quiet", "{kind}" }
        }
    }
}

/// One item of a paired server's, as cached: what clients get of it, its
/// children, and the external ids watch state is linked by.
#[component]
pub fn PairedItem(library: String, id: String) -> Element {
    let mut found = use_resource(use_reactive((&id,), |(id,)| load_item(id)));
    let current = found.read().clone();
    let Some(loaded) = current else {
        return rsx! { Skeleton { rows: 3 } };
    };
    let seen = match loaded {
        Ok(seen) => seen,
        Err(error) => {
            return rsx! {
                div { class: "flex flex-col gap-[22px]",
                    Alert { text: error.explain() }
                    span {
                        button { class: "btn", onclick: move |_| found.restart(), icons::RotateCcw {} "Try again" }
                    }
                }
            };
        }
    };
    let kind = item_kind(&seen.item);
    let ids: Vec<_> = seen
        .external_ids
        .iter()
        .filter(|i| !i.starts_with("plex://"))
        .map(|i| external_id(i, kind))
        .collect();
    let summary = match &seen.item {
        Item::Movie(v) => v.summary.clone(),
        Item::Show(v) => v.summary.clone(),
        Item::Episode(v) => v.summary.clone(),
        Item::Artist(v) => v.summary.clone(),
        _ => None,
    };
    let rating = match &seen.item {
        Item::Movie(v) => v.content_rating.clone(),
        Item::Show(v) => v.content_rating.clone(),
        Item::Episode(v) => v.content_rating.clone(),
        _ => None,
    };
    let genres = match &seen.item {
        Item::Show(v) => v.genres.join(", "),
        _ => String::new(),
    };
    let library_crumb = seen
        .library
        .as_ref()
        .map(|l| (l.id.clone(), l.title.clone()));
    let title = row_title(&seen.item);
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            Trail { server: seen.server_name.clone(), library: library_crumb, here: Some(title.clone()) }
            div { class: "cat-head",
                Thumb { item: seen.item.clone() }
                div { class: "flex min-w-0 flex-grow flex-col gap-[3px]",
                    h1 { class: "page-title page-title-admin", "{title}" }
                    span { class: "cat-quiet", "{item_context(&seen.item)}" }
                    Reach { online: seen.online }
                }
                span { class: "cat-readonly", icons::Eye { size: 13 } "Read-only · from {seen.server_name}" }
            }
            section { class: "card flex flex-col gap-3",
                if let Some(summary) = summary {
                    p { class: "m-0", "{summary}" }
                }
                if let Some(rating) = rating {
                    span { class: "cat-quiet", "Rated {rating}" }
                }
                if !genres.is_empty() {
                    span { class: "cat-quiet", "{genres}" }
                }
                if !ids.is_empty() {
                    div { class: "flex flex-wrap gap-2",
                        for x in ids {
                            if let Some(url) = x.url.clone() {
                                a { key: "{x.value}", class: "cat-chip", href: "{url}", target: "_blank", rel: "noreferrer", "{x.label} {x.value}" }
                            } else {
                                span { key: "{x.value}", class: "cat-chip", "{x.label} {x.value}" }
                            }
                        }
                    }
                }
                span { class: "cat-quiet text-xs",
                    "As this server cached it from {seen.server_name}. Its files are on that server; it plays from there."
                }
            }
            if !seen.children.is_empty() {
                section { class: "cat-table-card",
                    table { class: "cat-table",
                        thead {
                            tr {
                                th { "Title" }
                                th { "Year" }
                                th { "Kind" }
                            }
                        }
                        tbody {
                            for child in seen.children.iter().cloned() {
                                PairedRow { key: "{child.id()}", library: library.clone(), item: child }
                            }
                        }
                    }
                }
            }
        }
    }
}
