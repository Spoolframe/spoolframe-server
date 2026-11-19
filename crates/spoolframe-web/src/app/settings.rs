//! Settings (the "Settings, reorganised" boards): a sub-nav of areas
//! beside the page, an Overview of every area, and a page an area
//! (`settings_area`). Federation's settings are a tab of the Federation
//! page, and path mapping is on the Libraries page; the sub-nav lists
//! them under "Kept elsewhere". The old tabs' addresses lead to their
//! areas.

use std::rc::Rc;

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, SchemaPage, SchemaPlace, SettingIndexEntry, Settings as Loaded, SettingsSchema,
};

use super::settings_area::{AreaPage, schema};
use super::{Alert, Route};
use crate::settings_layout::{moved_tab, summary};
use crate::settings_marks::Counts;

/// Every setting's label, help and area (`admin.settingsIndex`), once
/// loaded: a Reset dialog names a setting by it. Empty until then, and
/// in the wizard, which does not load it.
#[derive(Clone, Copy)]
pub(super) struct SettingsIndex(pub Signal<Vec<SettingIndexEntry>>);

impl SettingsIndex {
    /// `key`'s label, or the key while the index is not here.
    pub(super) fn label(&self, key: &str) -> String {
        let SettingsIndex(index) = self;
        index
            .read()
            .iter()
            .find(|i| i.key == key)
            .map_or_else(|| key.to_owned(), |i| i.label.clone())
    }
}

/// "Show advanced settings" (PX-22), for forms drawn in a page whose
/// advanced settings are all shown: a sign-in provider's file and scopes.
#[derive(Clone, Copy)]
pub(super) struct Advanced(pub Signal<bool>);

/// Whether the advanced settings are shown. A hook: call it where hooks
/// are called.
pub(super) fn show_advanced() -> bool {
    try_use_context::<Advanced>().is_some_and(|Advanced(on)| on())
}

/// Bumped when an area saves, so the sub-nav counts again.
#[derive(Clone, Copy)]
pub(super) struct Refresh(pub Signal<u32>);

/// What the sub-nav and Overview draw and count: the pages, the
/// settings, and the index.
#[derive(Clone, PartialEq)]
struct Known {
    schema: Rc<SettingsSchema>,
    settings: Loaded,
    index: Vec<SettingIndexEntry>,
}

async fn known() -> Result<Known, crate::logic::ApiError> {
    let schema = schema().await?;
    let settings = super::wizard::load_settings().await?;
    let client = super::rpc::client().await?;
    let index = client.settings_index().await.map_err(super::rpc::error)?;
    Ok(Known {
        schema,
        settings,
        index,
    })
}

/// The address of `page`.
pub(super) fn page_route(page: &SchemaPage) -> Route {
    match page.place {
        SchemaPlace::Settings => Route::Settings {
            tab: page.slug.clone(),
        },
        SchemaPlace::Federation => Route::Federation {
            tab: "settings".into(),
        },
        SchemaPlace::Libraries => Route::Libraries {},
        // A place a newer server added: Settings' Overview lists it.
        SchemaPlace::Unknown => Route::SettingsHome {},
    }
}

/// The pages under Settings, and those kept elsewhere.
fn split(schema: &SettingsSchema) -> (Vec<&SchemaPage>, Vec<&SchemaPage>) {
    schema
        .pages
        .iter()
        .partition(|p| p.place == SchemaPlace::Settings)
}

/// The sub-nav's line for `page`: "3 changed · 1 restart".
fn counts_line(known: Option<&Known>, page: &SchemaPage) -> Option<String> {
    let k = known?;
    Counts::in_area(&k.settings.entries, &k.index, page.area).line()
}

/// The areas beside each settings page, with what changed in each.
#[component]
fn SubNav(here: String) -> Element {
    let refresh = try_use_context::<Refresh>();
    let state = use_resource(move || async move {
        if let Some(Refresh(r)) = refresh {
            let _ = r();
        }
        known().await.ok()
    });
    let known = state.read().clone().flatten();
    let link = |to: Route, name: &str, line: Option<String>, current: bool| {
        rsx! {
            super::settings_area::GuardedLink {
                class: if current { "subnav-link subnav-link-here" } else { "subnav-link" },
                aria_current: current.then(|| "page".to_owned()),
                to,
                span { "{name}" }
                if let Some(line) = line {
                    span { class: "subnav-count", "{line}" }
                }
            }
        }
    };
    let (areas, elsewhere) = known.as_ref().map(|k| split(&k.schema)).unwrap_or_default();
    rsx! {
        nav { class: "subnav", aria_label: "Settings areas",
            {link(Route::SettingsHome {}, "Overview", None, here.is_empty())}
            for p in areas {
                {link(page_route(p), &p.name, counts_line(known.as_ref(), p), here == p.slug)}
            }
            span { class: "subnav-head", "Kept elsewhere" }
            for p in elsewhere {
                {link(page_route(p), &p.name, counts_line(known.as_ref(), p), false)}
            }
            {link(Route::Users {}, "Per-user settings", None, false)}
        }
    }
}

/// The frame of every settings page: its sub-nav beside it.
#[component]
fn Frame(here: String, children: Element) -> Element {
    let refresh = use_signal(|| 0u32);
    use_context_provider(|| Refresh(refresh));
    rsx! {
        div { class: "settings-frame",
            SubNav { here: here.clone() }
            div { class: "flex min-w-0 flex-[1_1_480px] flex-col gap-[22px]", {children} }
        }
    }
}

/// Settings' Overview: what changed, what waits for a restart, and a
/// card for each area.
#[component]
pub fn SettingsHome() -> Element {
    let state = use_resource(known);
    let current = state.read().clone();
    rsx! {
        Frame { here: String::new(),
            div { class: "flex flex-col gap-1",
                h1 { class: "page-title page-title-admin", "Settings" }
                span { class: "text-sm text-ctp-subtext0", "Only what is set, and where. Live activity stays on the admin Overview." }
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(k)) => rsx! { Overview { known: k } },
            }
        }
    }
}

#[component]
fn Overview(known: Known) -> Element {
    let entries = &known.settings.entries;
    let all = Counts::of(entries);
    let waiting: Vec<String> = entries
        .iter()
        .filter(|e| e.restart_pending)
        .map(|e| {
            known
                .index
                .iter()
                .find(|i| i.key == e.key)
                .map_or_else(|| e.key.clone(), |i| i.label.clone())
        })
        .collect();
    let (areas, elsewhere) = split(&known.schema);
    let card = |p: &SchemaPage| {
        let line = Counts::in_area(entries, &known.index, p.area).line();
        let said = summary(&p.slug, entries);
        rsx! {
            Link { key: "{p.slug}", class: "area-card", to: page_route(p),
                b { "{p.name}" }
                if !said.is_empty() {
                    span { class: "text-sm", "{said}" }
                }
                span { class: "text-xs text-ctp-subtext0", "{p.line}" }
                if let Some(line) = line {
                    span { class: "area-card-count", "{line}" }
                }
            }
        }
    };
    rsx! {
        if let Some(strip) = all.strip() {
            div { class: "saved-strip", role: "status",
                span {
                    "{strip}"
                    if !waiting.is_empty() {
                        " ({waiting.join(\", \")})"
                    }
                }
                if !waiting.is_empty() {
                    super::restart::RestartButton { label: "Restart now", class: "btn-dialog btn-dialog-primary" }
                }
            }
        }
        h2 { class: "m-0 text-lg font-semibold", "Areas" }
        div { class: "area-cards",
            for p in areas {
                {card(p)}
            }
        }
        h2 { class: "m-0 text-lg font-semibold", "Kept elsewhere" }
        div { class: "area-cards",
            for p in elsewhere {
                {card(p)}
            }
            Link { class: "area-card", to: Route::Users {},
                b { "Per user" }
                span { class: "text-xs text-ctp-subtext0", "On each user’s page: subtitle downloads, PIN, device trust." }
            }
        }
    }
}

/// One area's page, or where an old tab's address now leads.
#[component]
pub fn Settings(tab: String) -> Element {
    let nav = navigator();
    let advanced = use_signal(|| true);
    use_context_provider(|| Advanced(advanced));
    let pages = use_resource(schema);
    let read = pages.read();
    let schema = match &*read {
        Some(Ok(schema)) => schema.clone(),
        Some(Err(e)) => {
            return rsx! {
                Frame { here: tab.clone(), Alert { text: e.explain() } }
            };
        }
        None => {
            return rsx! {
                Frame { here: tab.clone(), p { class: "text-ctp-subtext0", "Loading…" } }
            };
        }
    };
    let find = |slug: &str| schema.pages.iter().find(|p| p.slug == slug);
    let Some(here) = find(&tab).filter(|p| p.place == SchemaPlace::Settings) else {
        // A page kept elsewhere, or an old tab's name.
        let to = find(&tab)
            .or_else(|| moved_tab(&tab).and_then(find))
            .map_or(Route::SettingsHome {}, page_route);
        nav.replace(to);
        return rsx! {};
    };
    rsx! {
        Frame { here: here.slug.clone(),
            div { class: "flex flex-col gap-1",
                span { class: "text-sm text-ctp-subtext0", "Settings › {here.name}" }
                h1 { class: "page-title page-title-admin", "{here.name}" }
                span { class: "text-sm text-ctp-subtext0", "{here.line}" }
            }
            // A page of its own each: nothing edited goes with it.
            for slug in [here.slug.clone()] {
                AreaPage { key: "{slug}", slug }
            }
            // What the server does with them now, below the fields.
            if here.slug == "monitoring" {
                super::settings_extras::OtlpStatus {}
            }
            if here.slug == "subtitles" {
                section { class: "card", aria_label: "Today",
                    div { class: "card-body",
                        super::settings_extras::UsageToday {}
                    }
                }
            }
        }
    }
}

/// Federation's Settings tab: its switch and limits, drawn as an area.
#[component]
pub(super) fn FederationSettings() -> Element {
    rsx! {
        AreaPage { slug: "federation" }
    }
}
