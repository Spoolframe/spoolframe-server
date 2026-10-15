//! Open source licenses: every component the server and this web app are
//! built from, with its license and the license's text, opened from
//! Server's This server card. Laid out as the Linux client's Settings ›
//! About › Open source licenses: a search field, license chips, the
//! components by group, and a component's text with its copyright lines.
//!
//! The list is an asset the page asks for when it opens rather than part
//! of the wasm: 850 KB that no other page needs.

use dioxus::prelude::*;
use gloo_net::http::Request;

use super::admin::CopyButton;
use super::{Alert, Route, icons};
use crate::licenses::{Chip, Component, Group, Licenses, choice_note, choices, summary};
use crate::logic::ApiError;

const LIST: Asset = asset!("/assets/licenses.json");

async fn load() -> Result<Licenses, ApiError> {
    let unreachable = |e: gloo_net::Error| ApiError::transport(e.to_string());
    let response = Request::get(&LIST.to_string())
        .send()
        .await
        .map_err(unreachable)?;
    if !response.ok() {
        return Err(ApiError::transport(format!("HTTP {}", response.status())));
    }
    let bytes = response.binary().await.map_err(unreachable)?;
    Licenses::parse(&bytes).map_err(|e| ApiError::transport(e.to_string()))
}

/// `open`: the component whose text is shown (`name@version`), or empty
/// for the list. The search and the chip stay as they were when a text
/// is closed: the route's component is the same one.
#[component]
pub fn LicensesPage(open: String) -> Element {
    let list = use_resource(load);
    let mut query = use_signal(String::new);
    let mut chip = use_signal(|| Chip::All);
    let read = list.read();
    let licenses = match &*read {
        None => {
            return rsx! {
                LicensesHead {}
                p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" }
            };
        }
        Some(Err(e)) => {
            return rsx! {
                LicensesHead {}
                Alert { text: e.explain() }
            };
        }
        Some(Ok(licenses)) => licenses,
    };
    if !open.is_empty() {
        return match licenses.find(&open) {
            Some(component) => {
                let texts = choices(&component.license)
                    .into_iter()
                    .map(|choice| {
                        let text = licenses.shown_text(component, &choice);
                        (choice, text)
                    })
                    .collect();
                rsx! {
                    LicenseText { key: "{open}", component: component.clone(), texts }
                }
            }
            None => rsx! {
                div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
                    BackToList {}
                    Alert { text: format!("{open} is not in this server's list.") }
                }
            },
        };
    }
    let shown = licenses.shown(&query(), chip());
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            LicensesHead {}
            p { class: "m-0 max-w-[720px] text-sm text-ctp-subtext1",
                "{summary(licenses.components().len())}"
            }
            div { class: "flex flex-col gap-3",
                form { class: "cat-search", role: "search", onsubmit: move |e| e.prevent_default(),
                    icons::Search { size: 16 }
                    input {
                        r#type: "search",
                        aria_label: "Search components",
                        placeholder: "Search by name or license",
                        value: "{query}",
                        oninput: move |e| query.set(e.value()),
                    }
                }
                div { class: "cat-chips", role: "radiogroup", aria_label: "License",
                    for (c, n) in licenses.chips() {
                        button {
                            key: "{c.label()}",
                            class: if c == chip() { "cat-chip cat-chip-on" } else { "cat-chip" },
                            role: "radio",
                            aria_checked: "{c == chip()}",
                            onclick: move |_| chip.set(c),
                            "{c.label()}"
                            span { class: "cat-chip-count", "{n}" }
                        }
                    }
                }
            }
            if shown.is_empty() {
                section { class: "card",
                    div { class: "empty",
                        icons::Search { size: 28 }
                        b { "Nothing matches" }
                        span { "Try another name or license, or the All chip." }
                    }
                }
            }
            for (group, rows) in shown {
                div { key: "{group.title()}", class: "flex flex-col gap-2.5",
                    div { class: "section-head",
                        h2 { "{group.title()}" }
                        span { "{group.blurb()} {rows.len()} shown." }
                    }
                    div { class: "table-card",
                        table { class: "table",
                            thead {
                                tr {
                                    th { "Name" }
                                    th { class: "max-md:hidden", "Version" }
                                    th { "License" }
                                    if group != Group::Browser {
                                        th { class: "max-md:hidden", "Ships in" }
                                    }
                                }
                            }
                            tbody {
                                for i in rows {
                                    {
                                        let c = &licenses.components()[i];
                                        rsx! {
                                            tr { key: "{c.key()}",
                                                td {
                                                    Link { class: "license-name", to: Route::LicensesPage { open: c.key() }, "{c.name}" }
                                                }
                                                td { class: "quiet mono max-md:hidden", "{c.version}" }
                                                td { class: "quiet", "{c.license}" }
                                                if group != Group::Browser {
                                                    td { class: "quiet whitespace-nowrap max-md:hidden", "{c.ships_line()}" }
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
        }
    }
}

#[component]
fn LicensesHead() -> Element {
    rsx! {
        div { class: "flex flex-col gap-2",
            Link { class: "back-link", to: Route::Server {},
                icons::ChevronLeft {}
                "Server"
            }
            h1 { class: "page-title page-title-admin", "Open source licenses" }
        }
    }
}

#[component]
fn BackToList() -> Element {
    rsx! {
        Link { class: "back-link", to: Route::LicensesPage { open: String::new() },
            icons::ChevronLeft {}
            "Open source licenses"
        }
    }
}

/// One component's license text. `texts`: each way to take it under its
/// expression, with the text shown for it; a toggle picks one when there
/// are several.
#[component]
fn LicenseText(component: Component, texts: Vec<(Vec<String>, String)>) -> Element {
    let mut picked = use_signal(|| 0usize);
    let ways: Vec<Vec<String>> = texts.iter().map(|(choice, _)| choice.clone()).collect();
    let note = choice_note(&ways);
    let text = texts
        .get(picked())
        .map(|(_, text)| text.clone())
        .unwrap_or_default();
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            BackToList {}
            div { class: "flex flex-col gap-1",
                h1 { class: "page-title page-title-admin break-words", "{component.name}" }
                span { class: "text-sm text-ctp-subtext0",
                    span { class: "mono", "{component.version}" }
                    " · {component.license}"
                }
            }
            section { class: "card",
                div { class: "card-body",
                    if let Some(lines) = component.copyright_line() {
                        p { class: "m-0 whitespace-pre-line text-sm text-ctp-subtext1", "{lines}" }
                    }
                    if ways.len() > 1 || note.is_some() {
                        div { class: "flex flex-wrap items-center gap-2.5",
                            if ways.len() > 1 {
                                div { class: "seg", role: "radiogroup", aria_label: "License to read",
                                    for (i, way) in ways.iter().enumerate() {
                                        button {
                                            key: "{i}",
                                            class: if i == picked() { "seg-item seg-item-on" } else { "seg-item" },
                                            role: "radio",
                                            aria_checked: "{i == picked()}",
                                            onclick: move |_| picked.set(i),
                                            "{way.join(\" AND \")}"
                                        }
                                    }
                                }
                            }
                            if let Some(note) = note {
                                span { class: "text-[13px] text-ctp-subtext0", "{note}" }
                            }
                        }
                    }
                    pre { class: "license-text", "{text}" }
                    div {
                        CopyButton { text: text.clone(), label: "Copy text" }
                    }
                }
            }
        }
    }
}
