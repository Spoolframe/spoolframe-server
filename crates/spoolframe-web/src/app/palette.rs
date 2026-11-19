//! Search and commands (the WebCommandPalette board): Ctrl K / ⌘K on any
//! admin page, or the box at the top of the page, opens one dialog that
//! finds settings, library items, people, devices, paired servers and
//! admin pages, and runs the admin API's actions. What it finds and in
//! what order is `crate::palette`; this is the dialog and where a hit
//! leads.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminAction, AdminActionKind, AdminApiClient, CatalogHit, FederationApiClient,
};

use super::admin::sleep;
use super::{Route, icons, rpc};
use crate::palette::{
    Filter, Group, Hit, Page, People, RECENT_KEY, Results, Target, action_hits, admin_hits,
    library_hits, marked, recent_from, recent_to, remember, setting_hits, shortcut, suggested,
};

/// Whether the dialog is open: the box and the shortcut open it.
#[derive(Clone, Copy)]
pub(super) struct PaletteOpen(pub Signal<bool>);

/// A setting to show once Settings is open on its tab: the field's
/// anchor, and whether it is an advanced one, which turns "Show advanced
/// settings" on. Settings scrolls to it, marks it, and clears this.
#[derive(Clone, Copy)]
pub(super) struct SettingJump(pub Signal<Option<(String, bool)>>);

/// "Check media paths" was picked: Server's Media paths card runs the
/// check when it shows, and clears this.
#[derive(Clone, Copy)]
pub(super) struct CheckPathsAsked(pub Signal<bool>);

/// A group as the list draws it: its hits, how many, and each hit's
/// place in Up and Down's order.
type Shown = (Group, usize, Vec<(usize, Hit)>);

/// How many library hits one search asks for.
const LIBRARY_LIMIT: u32 = 20;

/// How long typing pauses before the libraries are searched, in ms.
const SEARCH_PAUSE_MS: i32 = 200;

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn kept_recent() -> Vec<Hit> {
    let kept = storage().and_then(|s| s.get_item(RECENT_KEY).ok().flatten());
    recent_from(kept.as_deref())
}

fn keep_recent(recent: &[Hit]) {
    if let Some(s) = storage() {
        let _ = s.set_item(RECENT_KEY, &recent_to(recent));
    }
}

/// The shortcut as this computer names it.
fn shortcut_here() -> &'static str {
    let platform = web_sys::window()
        .and_then(|w| w.navigator().platform().ok())
        .unwrap_or_default();
    shortcut(&platform)
}

/// Scroll to the field `anchor` marks, and mark it for a moment.
pub(super) fn show_field(anchor: &str) {
    super::dom::show_field(anchor);
}

/// The dialog's host, in the admin frame: the shortcut, and the restart
/// an action asked for. Once per frame.
#[component]
pub(super) fn PaletteHost(federation: bool) -> Element {
    let PaletteOpen(mut open) = use_context::<PaletteOpen>();
    let mut restarting = use_signal(|| false);
    // Ctrl K / ⌘K anywhere on the page, by a listener on the window
    // that goes with this frame. It runs outside Dioxus' own events, so
    // the toggle is made in this component's scope.
    use_hook(move || {
        let runtime = dioxus::core::Runtime::current();
        let scope = dioxus::core::current_scope_id();
        std::rc::Rc::new(super::dom::listen("keydown", move |e| {
            if super::dom::is_palette_key(&e) {
                e.prevent_default();
                runtime.in_scope(scope, || open.toggle());
            }
        }))
    });
    rsx! {
        if open() {
            Palette {
                federation,
                on_close: move |_| open.set(false),
                on_restart: move |_| {
                    open.set(false);
                    restarting.set(true);
                },
            }
        }
        if restarting() {
            super::restart::RestartDialog { on_cancel: move |_| restarting.set(false) }
        }
    }
}

/// The box at the top of each admin page: opens the dialog.
#[component]
pub(super) fn PaletteBox() -> Element {
    let PaletteOpen(mut open) = use_context::<PaletteOpen>();
    let keys = shortcut_here();
    rsx! {
        button {
            class: "palette-box",
            r#type: "button",
            aria_label: "Search, or press {keys}",
            onclick: move |_| open.set(true),
            icons::Search { size: 16 }
            span { class: "flex-grow truncate", "Search settings, the library, people…" }
            kbd { class: "kbd", "{keys}" }
        }
    }
}

/// The phone bar's way in: the same dialog, from an icon.
#[component]
pub(super) fn PaletteIcon() -> Element {
    let PaletteOpen(mut open) = use_context::<PaletteOpen>();
    rsx! {
        button { class: "text-ctp-subtext0", aria_label: "Search", onclick: move |_| open.set(true),
            icons::Search { size: 18 }
        }
    }
}

/// What the Admin group searches, read once the dialog opens.
async fn people(federation: bool) -> People {
    let Ok(client) = rpc::client().await else {
        return People::default();
    };
    let mut users = Vec::new();
    for user in client.user_list().await.unwrap_or_default() {
        let devices = client
            .user_devices(user.username.clone())
            .await
            .unwrap_or_default();
        users.push((user, devices));
    }
    let peers = if federation {
        client
            .peer_list()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.id, p.alias.unwrap_or(p.name)))
            .collect()
    } else {
        Vec::new()
    };
    People { users, peers }
}

/// The page a hit of [`Target::Page`] opens.
fn page_route(page: Page) -> Route {
    match page {
        Page::Overview => Route::AdminHome {},
        Page::Users => Route::Users {},
        Page::Invites => Route::Invites {},
        Page::Federation => Route::Federation { tab: String::new() },
        Page::Server => Route::Server {},
        Page::Activity => Route::ActivityPage {},
        Page::Playback => Route::PlaybackPage {},
        Page::Optimized => Route::OptimizedPage {},
        Page::Logs => Route::LogsPage {},
        Page::Libraries => Route::Libraries {},
        Page::Playlists => Route::ServerPlaylists {},
        Page::Settings => Route::SettingsHome {},
        Page::Audit => Route::AuditPageView {},
        Page::Diagnostics => Route::DiagnosticsPage {},
        Page::Licenses => Route::LicensesPage {
            open: String::new(),
        },
        Page::Profile => Route::Profile {},
    }
}

/// A hit's icon, by what it is.
fn hit_icon(hit: &Hit) -> Element {
    match &hit.target {
        Target::Setting { .. } => rsx! { icons::Sliders { size: 16 } },
        Target::Item { .. } => rsx! { icons::Film { size: 16 } },
        Target::User { .. } if hit.tag == "Device" => rsx! { icons::Laptop { size: 16 } },
        Target::User { .. } => rsx! { icons::User { size: 16 } },
        Target::Peer { .. } => rsx! { icons::Server { size: 16 } },
        Target::Page(_) => rsx! { icons::ArrowRight { size: 16 } },
        Target::Action {
            kind: AdminActionKind::Restart,
            ..
        } => {
            rsx! { icons::RotateCcw { size: 16 } }
        }
        Target::Action {
            kind: AdminActionKind::FindSubtitles,
            ..
        } => {
            rsx! { icons::Captions { size: 16 } }
        }
        Target::Action { .. } => rsx! { icons::Play { size: 16 } },
    }
}

/// The dialog: the box, the chips, the list, and the keys under it.
#[component]
fn Palette(federation: bool, on_close: EventHandler<()>, on_restart: EventHandler<()>) -> Element {
    // Every hit asks first if it leaves unsaved edits behind; a field of
    // the area open now does not, as its draft stays.
    let go = super::settings_area::use_guarded_push();
    let mut query = use_signal(String::new);
    let mut filter = use_signal(Filter::default);
    let mut selected = use_signal(|| 0usize);
    let mut recent = use_signal(kept_recent);
    let SettingJump(mut jump) = use_context::<SettingJump>();
    let CheckPathsAsked(mut check) = use_context::<CheckPathsAsked>();
    // What everything but the libraries is searched in, read as it opens.
    let index = use_resource(|| async {
        let client = rpc::client().await.ok()?;
        client.settings_index().await.ok()
    });
    let settings = use_resource(super::wizard::load_settings);
    // Which page each area's settings are on.
    let schema = use_resource(super::settings_area::schema);
    let actions = use_resource(|| async {
        let client = rpc::client().await.ok()?;
        client.admin_actions().await.ok()
    });
    let people = use_resource(move || people(federation));
    // The libraries, a moment after typing stops; the answer to an older
    // query is dropped.
    let library = use_resource(move || async move {
        let asked = query();
        if asked.trim().is_empty() {
            return Vec::<CatalogHit>::new();
        }
        sleep(SEARCH_PAUSE_MS).await;
        if *query.peek() != asked {
            return Vec::new();
        }
        let Ok(client) = rpc::client().await else {
            return Vec::new();
        };
        client
            .catalog_search(asked, LIBRARY_LIMIT)
            .await
            .unwrap_or_default()
    });
    let actions_now: Vec<AdminAction> = actions.read().clone().flatten().unwrap_or_default();
    let typed = query();
    let empty = typed.trim().is_empty();
    let results = if empty {
        // Before anything is typed: recent hits, then the actions.
        let mut groups = Vec::new();
        if !recent.read().is_empty() {
            groups.push((Group::Settings, recent.read().clone()));
        }
        let offered = suggested(&actions_now);
        if !offered.is_empty() {
            groups.push((Group::Actions, offered));
        }
        Results {
            groups,
            counts: Vec::new(),
        }
    } else {
        let mut hits = Vec::new();
        if let (Some(Some(index)), Some(Ok(settings))) = (&*index.read(), &*settings.read()) {
            hits.extend(setting_hits(&typed, index, &settings.entries));
        } else if let Some(Some(index)) = &*index.read() {
            hits.extend(setting_hits(&typed, index, &[]));
        }
        let library = library_hits(&library.read().clone().unwrap_or_default());
        hits.extend(action_hits(&typed, &actions_now, &library));
        hits.extend(library);
        if let Some(people) = &*people.read() {
            hits.extend(admin_hits(&typed, people));
        }
        Results::of(hits, filter())
    };
    let flat: Vec<Hit> = results.flat().into_iter().cloned().collect();
    let at = selected().min(flat.len().saturating_sub(1));
    let searching = !empty && library.read().is_none();

    let open_hit = use_callback(move |hit: Hit| {
        let kept = remember(recent.peek().clone(), &hit);
        keep_recent(&kept);
        recent.set(kept);
        match hit.target {
            Target::Setting {
                area,
                anchor,
                advanced,
                ..
            } => {
                jump.set(Some((anchor, advanced)));
                let page = schema
                    .peek()
                    .as_ref()
                    .and_then(|s| s.as_ref().ok())
                    .and_then(|s| s.pages.iter().find(|p| p.area == area))
                    .map(super::settings::page_route);
                go.call(page.unwrap_or(Route::SettingsHome {}));
            }
            Target::Item { library, id } => {
                go.call(Route::CatalogItem {
                    library,
                    id,
                    tab: String::new(),
                });
            }
            Target::User { username } => {
                go.call(Route::UserSessions { username });
            }
            Target::Peer { id } => {
                go.call(Route::FederationPeer { id });
            }
            Target::Page(page) => {
                go.call(page_route(page));
            }
            Target::Action { kind, item } => match kind {
                AdminActionKind::AddSource => {
                    go.call(Route::AddLibrary {
                        back: String::new(),
                    });
                }
                AdminActionKind::Invite => {
                    go.call(Route::Invites {});
                }
                AdminActionKind::CheckPaths => {
                    check.set(true);
                    go.call(Route::Server {});
                }
                AdminActionKind::Restart => {
                    on_restart.call(());
                    return;
                }
                // Its subtitles card is on the item's page.
                AdminActionKind::FindSubtitles => {
                    if let Some((library, id, _)) = item {
                        go.call(Route::CatalogItem {
                            library,
                            id,
                            tab: String::new(),
                        });
                    }
                }
                AdminActionKind::Unknown => {}
            },
        }
        on_close.call(());
    });
    let flat_keys = flat.clone();
    let onkeydown = move |e: KeyboardEvent| {
        let n = flat_keys.len();
        match e.key() {
            Key::ArrowDown if n > 0 => {
                e.prevent_default();
                selected.set((at + 1) % n);
            }
            Key::ArrowUp if n > 0 => {
                e.prevent_default();
                selected.set((at + n - 1) % n);
            }
            Key::Enter => {
                e.prevent_default();
                if let Some(hit) = flat_keys.get(at) {
                    open_hit.call(hit.clone());
                }
            }
            Key::Tab => {
                e.prevent_default();
                filter.set(filter().next(e.modifiers().contains(Modifiers::SHIFT)));
                selected.set(0);
            }
            Key::Escape => {
                e.prevent_default();
                on_close.call(());
            }
            _ => {}
        }
    };
    let count_of = |group: Option<Group>| -> String {
        if empty {
            return String::new();
        }
        let n = match group {
            None => results.total(),
            Some(group) => results
                .counts
                .iter()
                .find(|(g, _)| *g == group)
                .map_or(0, |(_, n)| *n),
        };
        if n == 0 && group.is_some() {
            String::new()
        } else {
            n.to_string()
        }
    };
    // Each group's hits with their place in Up and Down's order.
    let mut row = 0usize;
    let groups: Vec<Shown> = results
        .groups
        .iter()
        .map(|(group, hits)| {
            let rows = hits
                .iter()
                .map(|hit| {
                    row += 1;
                    (row - 1, hit.clone())
                })
                .collect();
            (*group, hits.len(), rows)
        })
        .collect();
    rsx! {
        div { class: "scrim palette-scrim", onclick: move |_| on_close.call(()),
            section {
                class: "palette",
                role: "dialog",
                aria_label: "Search and commands",
                onclick: move |e| e.stop_propagation(),
                div { class: "palette-input", role: "combobox", aria_expanded: "true",
                    icons::Search { size: 20 }
                    input {
                        class: "flex-grow",
                        r#type: "text",
                        aria_label: "Search and commands",
                        placeholder: "Search settings, the library, people, or type an action",
                        autofocus: true,
                        onmounted: move |e| async move {
                            let _ = e.set_focus(true).await;
                        },
                        value: "{typed}",
                        oninput: move |e| {
                            query.set(e.value());
                            selected.set(0);
                        },
                        onkeydown,
                    }
                    kbd { class: "kbd", "Esc" }
                }
                div { class: "palette-chips", role: "tablist", aria_label: "Filter by group",
                    for chip in Filter::CHIPS {
                        button {
                            key: "{chip.name()}",
                            class: if chip == filter() { "palette-chip palette-chip-here" } else { "palette-chip" },
                            role: "tab",
                            aria_selected: "{chip == filter()}",
                            tabindex: "-1",
                            onclick: move |_| {
                                filter.set(chip);
                                selected.set(0);
                            },
                            "{chip.name()} "
                            span { class: "palette-count",
                                {count_of(match chip { Filter::All => None, Filter::Only(g) => Some(g) })}
                            }
                        }
                    }
                }
                div { class: "palette-list",
                    if !empty && flat.is_empty() && !searching {
                        div { class: "palette-none",
                            b { "Nothing matches “{typed.trim()}”" }
                            span {
                                "Settings, admin pages, people, devices, paired servers, actions and every library were searched."
                            }
                            span {
                                "Try fewer letters, or a setting’s name in the config, such as "
                                code { class: "mono-value", "transcode.max_jobs" }
                                "."
                            }
                        }
                    }
                    ul { role: "listbox", aria_label: "Results",
                        for (gi, (group, size, rows)) in groups.into_iter().enumerate() {
                            li { key: "{gi}", role: "presentation",
                                span { class: "palette-group",
                                    {
                                        match (empty, group) {
                                            (true, Group::Actions) => "Suggested actions".to_owned(),
                                            (true, _) => "Recent".to_owned(),
                                            (false, g) => format!("{} · {size}", g.name()),
                                        }
                                    }
                                }
                                ul { role: "group",
                                    for (n, hit) in rows {
                                        {
                                            let here = n == at;
                                            let pick = hit.clone();
                                            rsx! {
                                                li {
                                                    key: "{n}",
                                                    class: if here { "palette-row palette-row-here" } else { "palette-row" },
                                                    role: "option",
                                                    aria_selected: "{here}",
                                                    onmouseenter: move |_| selected.set(n),
                                                    onclick: move |_| open_hit.call(pick.clone()),
                                                    span { class: "palette-icon", {hit_icon(&hit)} }
                                                    span { class: "flex min-w-0 flex-grow flex-col gap-0.5",
                                                        span { class: "palette-title",
                                                            for (i, (piece, bold)) in marked(&hit.title, &typed).into_iter().enumerate() {
                                                                if bold {
                                                                    b { key: "{i}", "{piece}" }
                                                                } else {
                                                                    span { key: "{i}", "{piece}" }
                                                                }
                                                            }
                                                        }
                                                        span { class: "palette-detail", "{hit.detail}" }
                                                    }
                                                    if !hit.tag.is_empty() {
                                                        span { class: "palette-tag", "{hit.tag}" }
                                                    }
                                                    if here {
                                                        kbd { class: "kbd", "Enter" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "palette-keys",
                    span { kbd { class: "kbd", "↑" } kbd { class: "kbd", "↓" } " Move" }
                    span { kbd { class: "kbd", "Enter" } " Open" }
                    span { kbd { class: "kbd", "Tab" } " Filter by group" }
                    span { kbd { class: "kbd", "Esc" } " Close" }
                    span { class: "ml-auto", kbd { class: "kbd", "{shortcut_here()}" } " anywhere" }
                }
            }
        }
    }
}
