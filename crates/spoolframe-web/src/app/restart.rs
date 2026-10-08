//! Restarting the server from the web UI: the banner that says what waits
//! for a restart, and the button that asks for one, behind a confirm
//! dialog, then waits for the server to come back and loads the page anew
//! (which also brings a new web bundle).

use dioxus::prelude::*;
use spoolframe_admin_api::AdminApiClient;

use super::admin::sleep;
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{Alert, icons, rpc};
use crate::restart::{pending, pending_text, what_happens};

/// Every Settings tab's and the Server page's banner while restart
/// settings changed since the server started: how many, which, and
/// "Restart now". Nothing otherwise, or while the settings load.
#[component]
pub(super) fn PendingRestart() -> Element {
    let settings = use_resource(super::wizard::load_settings);
    let Some(Ok(loaded)) = settings.read().clone() else {
        return rsx! {};
    };
    let keys = pending(&loaded);
    if keys.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "banner", role: "status", style: "margin-bottom:0",
            icons::RotateCcw { size: 17 }
            span {
                b { "{pending_text(keys.len())}: " }
                span { class: "mono-value", {keys.join(", ")} }
            }
            RestartButton { label: "Restart now", class: "btn btn-teal" }
        }
    }
}

/// A button that restarts the server, once the confirm dialog says so.
/// `label` and `class` are the button's.
#[component]
pub(super) fn RestartButton(label: &'static str, class: &'static str) -> Element {
    let mut asking = use_signal(|| false);
    rsx! {
        button { class, r#type: "button", onclick: move |_| asking.set(true),
            icons::RotateCcw { size: 13 }
            "{label}"
        }
        if asking() {
            RestartDialog { on_cancel: move |_| asking.set(false) }
        }
    }
}

/// Where a restart asked for here is.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// The question, with why the last try failed, if it did.
    Asking(Option<String>),
    /// Asked; the server has not answered yet.
    Sending,
    /// The server stops and starts again: waiting for it.
    Restarting,
    /// It has not come back in time.
    Gone,
}

/// "Restart the server?", then "Confirm it's you" if the server asks for a
/// recent sign-in, then "Restarting…" until the server answers again.
#[component]
fn RestartDialog(on_cancel: EventHandler<()>) -> Element {
    let settings = use_resource(super::wizard::load_settings);
    let mut phase = use_signal(|| Phase::Asking(None));
    let confirming = use_signal(|| None::<String>);
    let go = use_callback(move |()| {
        phase.set(Phase::Sending);
        spawn(async move {
            let asked = async {
                let client = rpc::client().await?;
                client.restart().await.map_err(rpc::error)?;
                Ok(client)
            };
            match asked.await {
                Ok(client) => {
                    phase.set(Phase::Restarting);
                    if !come_back(&client).await {
                        phase.set(Phase::Gone);
                    }
                }
                Err(e) if asks_to_confirm(&e, confirming, "restarting the server") => {
                    phase.set(Phase::Asking(None));
                }
                Err(e) => phase.set(Phase::Asking(Some(e.explain()))),
            }
        });
    });
    let mode = settings
        .read()
        .clone()
        .and_then(Result::ok)
        .map(|s| s.restart_mode);
    let said = mode.map_or("Connected devices drop for a few seconds.", what_happens);
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                match phase() {
                    Phase::Asking(_) | Phase::Sending => rsx! {
                        div { class: "dialog-head dialog-head-warn",
                            icons::RotateCcw { size: 22 }
                            h1 { "Restart the server?" }
                        }
                        p { "{said}" }
                        if let Phase::Asking(Some(text)) = phase() {
                            Alert { text }
                        }
                        div { class: "dialog-actions",
                            button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_cancel.call(()), "Cancel" }
                            button {
                                class: "btn-dialog btn-dialog-primary",
                                disabled: phase() == Phase::Sending,
                                onclick: move |_| go.call(()),
                                "Restart"
                            }
                        }
                    },
                    Phase::Restarting => rsx! {
                        div { class: "dialog-head",
                            icons::RotateCcw { size: 22 }
                            h1 { "Restarting…" }
                        }
                        p { "Waiting for the server to answer again. This page loads again when it does." }
                    },
                    Phase::Gone => rsx! {
                        div { class: "dialog-head dialog-head-danger",
                            icons::Warning { size: 22 }
                            h1 { "The server has not come back" }
                        }
                        p { "It did not answer within a minute. Check its log." }
                        div { class: "dialog-actions",
                            button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_cancel.call(()), "Close" }
                        }
                    },
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| go.call(()) }
    }
}

/// Wait for the server `client` is connected to to stop, then to answer
/// again, and load the page anew. False if it is not back within about a
/// minute.
async fn come_back(client: &jsonrpsee::wasm_client::Client) -> bool {
    // Gone first: until its socket closes, the old server would answer.
    for _ in 0..75 {
        if !client.is_connected() {
            break;
        }
        sleep(200).await;
    }
    for _ in 0..60 {
        if super::http::site().await.is_ok() {
            rpc::forget();
            if let Some(window) = web_sys::window() {
                let _ = window.location().reload();
            }
            return true;
        }
        sleep(1000).await;
    }
    false
}
