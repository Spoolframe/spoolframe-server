//! Apps connected through MCP (PX-42): My profile › Connected apps, the
//! same list for one user on their admin page, and Server › Audit, the
//! audit list.
//!
//! Disconnecting is immediate and final, after a confirmation: there is
//! no undo, and reconnecting means signing in and consenting again.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, AuditPage, ConnectedApp, MeApiClient};
use wasm_bindgen::JsValue;

use super::admin::{Said, Toast, say};
use super::{Alert, icons, now, rpc};
use crate::connect::{audit_action, calls_line, scope_name};
use crate::logic::{ApiError, ago};

/// Whose apps a list shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Whose {
    /// The signed-in user's own (My profile).
    Mine,
    /// A user's, on their admin page.
    User(String),
}

async fn load(whose: Whose) -> Result<Vec<ConnectedApp>, ApiError> {
    let client = rpc::client().await?;
    match whose {
        Whose::Mine => client.connected_apps().await.map_err(rpc::error),
        Whose::User(username) => client
            .all_connected_apps()
            .await
            .map(|apps| {
                apps.into_iter()
                    .filter(|a| a.username == username)
                    .collect()
            })
            .map_err(rpc::error),
    }
}

/// The card: each app, what it may do, its last call and calls today, and
/// Disconnect.
#[component]
pub fn ConnectedApps(whose: Whose) -> Element {
    let asked = whose.clone();
    let mut apps = use_resource(move || load(asked.clone()));
    let mut asking = use_signal(|| None::<ConnectedApp>);
    let toast = use_signal(|| None::<Said>);
    let current = apps.read().clone();
    let lede = match &whose {
        Whose::Mine => "AI agents and other apps you let act for you through MCP.".to_owned(),
        Whose::User(name) => {
            format!("AI agents and other apps {name} let act for them through MCP.")
        }
    };
    rsx! {
        section { class: "card",
            div { class: "card-head",
                icons::LinkChain {}
                h2 { "Connected apps" }
            }
            div { class: "card-body",
                p { class: "m-0 text-sm text-ctp-subtext0", "{lede}" }
                match current {
                    None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        p { class: "m-0 text-sm text-ctp-subtext0", "None." }
                    },
                    Some(Ok(list)) => rsx! {
                        for app in list {
                            AppRow { key: "{app.id}", app: app.clone(), on_revoke: move |app| asking.set(Some(app)) }
                        }
                    },
                }
            }
        }
        if let Some(app) = asking() {
            RevokeDialog {
                app,
                mine: whose == Whose::Mine,
                on_done: move |said: Option<Result<String, String>>| {
                    asking.set(None);
                    match said {
                        Some(Ok(text)) => {
                            say(toast, true, text);
                            apps.restart();
                        }
                        Some(Err(text)) => say(toast, false, text),
                        None => {}
                    }
                },
            }
        }
        Toast { said: toast }
    }
}

#[component]
fn AppRow(app: ConnectedApp, on_revoke: EventHandler<ConnectedApp>) -> Element {
    let scopes = app
        .scopes
        .iter()
        .map(|s| scope_name(s))
        .collect::<Vec<_>>()
        .join(", ");
    let last = match app.last_call_at {
        Some(at) => format!("Last call {}", ago(now(), at)),
        None => "No calls yet".to_owned(),
    };
    let revoking = app.clone();
    rsx! {
        div { class: "flex items-start justify-between gap-3",
            span { class: "flex min-w-0 flex-col gap-0.5",
                b { "“{app.client_name}”" }
                span { class: "text-[13px] text-ctp-subtext0",
                    "Returns to " b { "{app.return_host}" } " · {scopes}"
                }
                span { class: "text-[13px] text-ctp-subtext0", "{last} · {calls_line(app.calls_today)}" }
            }
            button { class: "btn btn-danger", onclick: move |_| on_revoke.call(revoking.clone()), "Disconnect" }
        }
    }
}

/// "Disconnect “Claude”?", saying it cannot be undone.
#[component]
fn RevokeDialog(
    app: ConnectedApp,
    mine: bool,
    on_done: EventHandler<Option<Result<String, String>>>,
) -> Element {
    let mut busy = use_signal(|| false);
    let id = app.id.clone();
    let name = app.client_name.clone();
    let go = move |_| {
        busy.set(true);
        let id = id.clone();
        let name = name.clone();
        spawn(async move {
            let revoked = async {
                let client = rpc::client().await?;
                if mine {
                    client.revoke_connected_app(id).await
                } else {
                    client.revoke_any_connected_app(id).await
                }
                .map_err(rpc::error)
            };
            let done = revoked
                .await
                .map(|()| format!("“{name}” disconnected"))
                .map_err(|e| e.explain());
            busy.set(false);
            on_done.call(Some(done));
        });
    };
    let whose = if mine { "your account" } else { "this account" };
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::Unlink { size: 22 }
                    h1 { "Disconnect “{app.client_name}”?" }
                }
                p {
                    "Its access to {whose} ends now, and this cannot be undone. To use it again, connect it from the app and allow it again."
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: go, "Disconnect" }
                }
            }
        }
    }
}

/// How many entries a page of the audit list shows.
const PAGE: u32 = 50;

async fn audit(offset: u32) -> Result<AuditPage, ApiError> {
    let client = rpc::client().await?;
    client.audit_list(offset, PAGE).await.map_err(rpc::error)
}

/// The browser's local date and time of Unix seconds `at`.
fn when(at: i64) -> String {
    js_sys::Date::new(&JsValue::from_f64(at as f64 * 1000.0))
        .to_locale_string("default", &JsValue::UNDEFINED)
        .into()
}

/// Server › Audit: what admins did through MCP clients, and which apps
/// were connected and disconnected, newest first, a page at a time.
#[component]
pub fn AuditPageView() -> Element {
    let mut offset = use_signal(|| 0u32);
    let page = use_resource(move || audit(offset()));
    let current = page.read().clone();
    rsx! {
        div { class: "flex flex-col gap-6",
            div { class: "flex flex-col gap-1.5",
                h1 { class: "page-title page-title-admin hidden md:block", "Audit" }
                span { class: "max-w-[680px] text-sm leading-normal text-ctp-subtext0",
                    "What admins did through AI agents and other MCP apps, and which apps users connected and disconnected, newest first. Every MCP call is also in the Logs, under “audit”."
                }
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                Some(Ok(page)) if page.events.is_empty() => rsx! {
                    section { class: "card empty", p { class: "m-0 text-ctp-subtext0", "Nothing yet." } }
                },
                Some(Ok(page)) => rsx! {
                    section { class: "card",
                        div { class: "card-body",
                            for event in page.events.iter() {
                                div { key: "{event.id}", class: "flex flex-col gap-0.5",
                                    span {
                                        b { "{event.actor}" }
                                        " · {audit_action(&event.action)}"
                                        if let Some(target) = &event.target {
                                            " · {target}"
                                        }
                                    }
                                    span { class: "text-[13px] text-ctp-subtext0",
                                        "{when(event.at)} · through {event.client} · {event.outcome}"
                                    }
                                }
                            }
                        }
                    }
                    div { class: "flex items-center gap-2",
                        button { class: "btn", disabled: offset() == 0, onclick: move |_| offset.set(offset().saturating_sub(PAGE)), icons::ChevronLeft {} "Newer" }
                        button { class: "btn", disabled: offset() + PAGE >= page.total, onclick: move |_| offset.set(offset() + PAGE), "Older" icons::ChevronRight {} }
                        span { class: "text-[13px] text-ctp-subtext0", "{page.total} in all" }
                    }
                },
            }
        }
    }
}
