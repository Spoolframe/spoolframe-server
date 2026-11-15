//! Server › Diagnostics: the reports apps sent from Settings › About ›
//! "Send diagnostics", newest first; one report's page with who sent it
//! from what, its note, and its log in a scrolling block, Download and
//! Delete, Delete asked first.

use super::catalog::Forbidden;
use super::connected::when;
use super::logs::save_file;
use super::{Alert, Route, icons, now_ms, rpc};
use crate::diagnostics::{device_line, download_name, download_text, size_line, software_line};
use crate::logic::{ApiError, ago};
use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, DiagnosticsDetail as Detail, DiagnosticsSummary};

async fn list() -> Result<Vec<DiagnosticsSummary>, ApiError> {
    let client = rpc::client().await?;
    client.diagnostics_list().await.map_err(rpc::error)
}

async fn one(id: String) -> Result<Detail, ApiError> {
    let client = rpc::client().await?;
    client.diagnostics(id).await.map_err(rpc::error)
}

/// Server › Diagnostics: every report kept, newest first.
#[component]
pub fn DiagnosticsPage() -> Element {
    let reports = use_resource(list);
    let nav = navigator();
    let now = now_ms() / 1000;
    let current = reports.read().clone();
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            div { class: "flex flex-col gap-1.5",
                h1 { class: "page-title page-title-admin hidden md:block", "Diagnostics" }
                span { class: "max-w-[680px] text-sm leading-normal text-ctp-subtext0",
                    "What apps sent from Settings › About › Send diagnostics: the device, the app and its playback log, newest first. Each device's newest 20 are kept, for 30 days."
                }
            }
            match current {
                None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) if e.is_forbidden() => rsx! { Forbidden { what: "diagnostics" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(rows)) if rows.is_empty() => rsx! {
                    section { class: "card empty",
                        icons::Download {}
                        b { "No reports" }
                        span { "When someone sends diagnostics from an app, they show here." }
                    }
                },
                Some(Ok(rows)) => rsx! {
                    div { class: "table-card",
                        table { class: "table",
                            thead {
                                tr {
                                    th { "Sent by" }
                                    th { "Device" }
                                    th { "App" }
                                    th { "Received" }
                                    th { "Log" }
                                }
                            }
                            tbody {
                                for row in rows {
                                    tr {
                                        key: "{row.id}",
                                        class: "opens",
                                        onclick: {
                                            let id = row.id.clone();
                                            move |_| { nav.push(Route::DiagnosticsReport { id: id.clone() }); }
                                        },
                                        td {
                                            div { class: "who-name",
                                                Link { class: "font-bold text-ctp-text no-underline", to: Route::DiagnosticsReport { id: row.id.clone() }, "{row.user_name}" }
                                                if row.has_note {
                                                    span { "with a note" }
                                                }
                                            }
                                        }
                                        td { "{device_line(&row)}" }
                                        td { class: "text-ctp-subtext1", "{software_line(&row)}" }
                                        td { title: "{when(row.received_at)}", "{ago(now, row.received_at)}" }
                                        td { class: "mono", "{size_line(&row)}" }
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

/// One report: who and what, the note, the log; Download and Delete.
#[component]
pub fn DiagnosticsReport(id: String) -> Element {
    let asked = id.clone();
    let report = use_resource(move || one(asked.clone()));
    let mut asking = use_signal(|| false);
    let mut saving = use_signal(|| None::<String>);
    let current = report.read().clone();
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            Link { class: "flex items-center gap-1 text-sm no-underline", to: Route::DiagnosticsPage {},
                icons::ChevronLeft { size: 14 }
                "Diagnostics"
            }
            match current {
                None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) if e.is_forbidden() => rsx! { Forbidden { what: "diagnostics" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(detail)) => {
                    let s = detail.summary.clone();
                    let saved = detail.clone();
                    let download = move |_| {
                        if let Err(e) = save_file(&download_name(&saved.summary), &download_text(&saved)) {
                            saving.set(Some(format!("The file could not be made: {e:?}")));
                        }
                    };
                    rsx! {
                        div { class: "page-head items-start",
                            div { class: "flex min-w-0 flex-col gap-1",
                                h1 { class: "page-title page-title-admin m-0", "{s.user_name} · {device_line(&s)}" }
                                span { class: "text-sm text-ctp-subtext0", "{when(s.received_at)} · report {s.id}" }
                            }
                            div { class: "flex gap-2",
                                button { class: "btn", onclick: download,
                                    icons::Download {}
                                    "Download .txt"
                                }
                                button { class: "btn btn-danger", onclick: move |_| asking.set(true),
                                    icons::X { size: 15 }
                                    "Delete"
                                }
                            }
                        }
                        if let Some(text) = saving() {
                            Alert { text }
                        }
                        section { class: "card",
                            dl { class: "diag-meta",
                                dt { "User" }
                                dd { "{s.user_name} " span { class: "mono text-xs text-ctp-overlay0", "{s.user_id}" } }
                                dt { "Device" }
                                dd { "{device_line(&s)} " span { class: "mono text-xs text-ctp-overlay0", "{s.endpoint_id}" } }
                                dt { "Software" }
                                dd { "{software_line(&s)}" }
                                dt { "Log" }
                                dd { "{size_line(&s)}" }
                                if let Some(note) = &detail.note {
                                    dt { "Note" }
                                    dd { class: "whitespace-pre-wrap", "{note}" }
                                }
                            }
                        }
                        if detail.log.is_empty() {
                            section { class: "log-box", div { class: "log-tail", "The app sent no log." } }
                        } else {
                            pre { class: "log-box diag-log", "{detail.log}" }
                        }
                        if asking() {
                            Confirm { summary: s.clone(), asking }
                        }
                    }
                }
            }
        }
    }
}

/// Delete asks first: there is no undo.
#[component]
fn Confirm(summary: DiagnosticsSummary, asking: Signal<bool>) -> Element {
    let mut asking = asking;
    let mut busy = use_signal(|| false);
    let mut failed = use_signal(|| None::<String>);
    let nav = navigator();
    let id = summary.id.clone();
    let go = move |_| {
        busy.set(true);
        let id = id.clone();
        spawn(async move {
            let done: Result<(), ApiError> = async {
                let client = rpc::client().await?;
                client.diagnostics_delete(id).await.map_err(rpc::error)
            }
            .await;
            busy.set(false);
            match done {
                Ok(()) => {
                    asking.set(false);
                    nav.replace(Route::DiagnosticsPage {});
                }
                Err(e) => failed.set(Some(e.explain())),
            }
        });
    };
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::X { size: 22 }
                    h1 { "Delete {summary.user_name}'s report?" }
                }
                p { "The report and its log are deleted from the server. There is no undo." }
                if let Some(why) = failed() {
                    Alert { text: why }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| asking.set(false), "Cancel" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: go, "Delete" }
                }
            }
        }
    }
}
