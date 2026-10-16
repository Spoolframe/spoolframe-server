//! Connecting an app (PX-42): the consent page an MCP client's
//! `/authorize` sends its user's browser to, `/connect?request=…`. Laid
//! out as the device approval page is (`device.rs`): sign in here if need
//! be, then the app, where it returns to, what it may do, and Deny or
//! Allow.
//!
//! The app's name is the one it registered, which anyone can choose, so
//! it is shown in quotes as the app's own word, and beside it, in bold,
//! the host its answer goes to: what a user can check.

use dioxus::prelude::*;

use super::{Alert, Centered, Notice, SignInForm, http, icons, plain_http_page, rpc};
use crate::connect;

#[component]
pub fn Connect(request: String) -> Element {
    let mut me = use_resource(http::me);
    let site = use_resource(super::site);
    let current = me.read().clone();
    let server = site
        .read()
        .as_ref()
        .map(|s| s.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Spoolframe".into());
    let public_url = site.read().as_ref().and_then(|s| s.public_url.clone());
    let insecure_cookies = site.read().as_ref().is_some_and(|s| s.insecure_cookies);
    let sso = site
        .read()
        .as_ref()
        .map(|s| s.sso.clone())
        .unwrap_or_default();
    rsx! {
        Centered {
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { div { class: "glass-card", Alert { text: e.explain() } } },
                Some(Ok(None)) => rsx! {
                    SignInForm {
                        title: "Sign in to {server}",
                        lede: "to connect an app",
                        submit_label: "Sign in and continue",
                        footer: "After signing in you come straight back to the app's request.",
                        public_url,
                        insecure_cookies,
                        sso,
                        return_to: connect::return_to(&request),
                        on_done: move |_| me.restart(),
                    }
                },
                Some(Ok(Some(user))) => rsx! {
                    Consent {
                        key: "{request}",
                        request: request.clone(),
                        who: user.display_name.clone().unwrap_or(user.username.clone()),
                        server,
                        on_switch: move |_| me.restart(),
                    }
                },
            }
        }
    }
}

/// The app, where it returns to, what it may do; Deny and Allow.
#[component]
fn Consent(request: String, who: String, server: String, on_switch: EventHandler<()>) -> Element {
    let asked = request.clone();
    let pending = use_resource(move || {
        let id = asked.clone();
        async move { http::connect_request(&id).await }
    });
    let mut unticked = use_signal(Vec::<String>::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let deciding = request.clone();
    let decide = use_callback(move |(allow, scopes): (bool, Vec<String>)| {
        let id = deciding.clone();
        busy.set(true);
        error.set(None);
        spawn(async move {
            match http::connect_decide(&id, allow, &scopes).await {
                // Back to the app, with its answer: the page is done.
                Ok(to) => http::go(&to),
                Err(e) => {
                    error.set(Some(e.explain()));
                    busy.set(false);
                }
            }
        });
    });
    let switch = move |_| {
        spawn(async move {
            let _ = http::logout().await;
            rpc::forget();
            on_switch.call(());
        });
    };
    let current = pending.read().clone();
    match current {
        None => rsx! {
            div { class: "glass-card approve-card",
                p { class: "m-0 text-center text-ctp-subtext0", "Loading…" }
            }
        },
        Some(Err(e)) => rsx! {
            section { class: "glass-card approve-card",
                CardHead { title: "This request is over" }
                Alert { text: e.explain() }
                p { class: "approve-who", "Start connecting again from the app." }
            }
        },
        Some(Ok(found)) => {
            let granted = connect::granted(&found.scopes, &unticked());
            let allowing = granted.clone();
            rsx! {
                section { class: "glass-card approve-card",
                    CardHead { title: "Connect an app to {server}?" }
                    p { class: "approve-who",
                        "An app that calls itself "
                        b { "“{found.client_name}”" }
                        " wants to use {server} as you. That name is the app's own: {server} cannot check it."
                    }
                    p { class: "approve-who",
                        "Its answer goes back to "
                        b { "{found.return_host}" }
                        ". Allow it only if that is the app you are connecting."
                    }
                    // PX-23: what is sent from here crosses the network in
                    // clear.
                    if plain_http_page() {
                        Notice {
                            "This page is plain HTTP across the network: your sign-in, and the app's access, can be read on the way. Connect apps through the web UI's HTTPS address."
                        }
                    }
                    for scope in found.scopes.iter().filter(|s| connect::scope_words(s).is_some()) {
                        ScopeBox {
                            key: "{scope}",
                            scope: scope.clone(),
                            ticked: !unticked().contains(scope),
                            disabled: busy(),
                            on_toggle: move |scope: String| {
                                let mut now = unticked();
                                match now.iter().position(|s| *s == scope) {
                                    Some(at) => {
                                        now.remove(at);
                                    }
                                    None => now.push(scope),
                                }
                                unticked.set(now);
                            },
                        }
                    }
                    p { class: "approve-who",
                        "You're signed in as " b { "{who}" } ". You can disconnect the app at any time in My profile."
                    }
                    if let Some(text) = error() {
                        Alert { text }
                    }
                    div { class: "grid grid-cols-2 gap-2.5",
                        button {
                            class: "btn-secondary btn-tall",
                            disabled: busy(),
                            onclick: move |_| decide.call((false, Vec::new())),
                            "Deny"
                        }
                        button {
                            class: "btn-primary btn-tall",
                            disabled: busy() || granted.is_empty(),
                            onclick: move |_| decide.call((true, allowing.clone())),
                            "Allow"
                        }
                    }
                    button { class: "btn-plain text-center text-[13px] text-ctp-teal", r#type: "button", onclick: switch,
                        "Not you? Use another account"
                    }
                }
            }
        }
    }
}

/// The mark and title the card starts with.
#[component]
fn CardHead(title: String) -> Element {
    rsx! {
        div { class: "approve-head",
            icons::Mark { size: 48 }
            h1 { "{title}" }
        }
    }
}

/// One scope: a box to tick, what it is called and what it lets the app
/// do.
#[component]
fn ScopeBox(
    scope: String,
    ticked: bool,
    disabled: bool,
    on_toggle: EventHandler<String>,
) -> Element {
    let (title, line) = connect::scope_words(&scope).unwrap_or(("", ""));
    let label = format!("scope-{}", scope.replace(':', "-"));
    let toggled = scope.clone();
    rsx! {
        div { class: "remember-box",
            button {
                class: if ticked { "box box-on" } else { "box" },
                r#type: "button",
                role: "checkbox",
                aria_checked: "{ticked}",
                aria_labelledby: "{label}",
                disabled,
                onclick: move |_| on_toggle.call(toggled.clone()),
                if ticked { icons::Check {} }
            }
            span { class: "flex flex-col gap-0.5",
                b { id: "{label}", "{title}" }
                span { class: "remember-line", "{line}" }
            }
        }
    }
}
