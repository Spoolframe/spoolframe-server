//! Approving a device's sign-in from a browser (P3-07, P3-05): the page a
//! device's QR code opens, `/auth/device?code=…`, as the design's
//! "Web · Browser consent" card. An app cannot use a passkey itself; a
//! phone's browser can, here, and then says yes for the app.

use dioxus::prelude::*;
use spoolframe_admin_api::{DeviceSignIn, MeApiClient, UserInfo};

use super::{Alert, Centered, Field, Route, SignInForm, http, icons, rpc};
use crate::logic::{ApiError, display_code};

/// Where the page is.
const PATH: &str = "/auth/device";

/// `text` as a query value.
fn encode(text: &str) -> String {
    js_sys::encode_uri_component(text).into()
}

#[component]
pub fn DeviceApproval(code: String) -> Element {
    // Who is signed in is asked first: without anyone, sign in here and
    // stay on the code.
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
    // A provider's sign-in comes back here, to this code, and says here
    // what went wrong.
    let said = use_hook(|| {
        http::sso_outcome(PATH)
            .and_then(|(what, provider)| crate::sso::login_message(&what, provider.as_deref()))
            .map(|(_, text)| text)
    });
    let return_to = format!("{PATH}?code={}", encode(&code));
    rsx! {
        Centered {
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { div { class: "glass-card", Alert { text: e.explain() } } },
                Some(Ok(None)) => rsx! {
                    SignInForm {
                        title: "Sign in to {server}",
                        public_url,
                        insecure_cookies,
                        sso,
                        said,
                        return_to,
                        on_done: move |_| me.restart(),
                    }
                },
                Some(Ok(Some(user))) if code.trim().is_empty() => rsx! {
                    EnterCode { user }
                },
                Some(Ok(Some(user))) => rsx! {
                    Consent { key: "{code}", user, code: code.clone(), server, on_switch: move |_| me.restart() }
                },
            }
        }
    }
}

/// Opened without a code: typed from what the device shows.
#[component]
fn EnterCode(user: UserInfo) -> Element {
    let nav = navigator();
    let code = use_signal(String::new);
    let submit = move |e: FormEvent| {
        e.prevent_default();
        nav.replace(Route::DeviceApproval { code: code() });
    };
    rsx! {
        form { class: "glass-card", onsubmit: submit,
            div { class: "flex flex-col items-center gap-2.5 text-center",
                icons::Mark { size: 52 }
                h1 { class: "m-0 text-2xl font-bold", "Sign in a device" }
                p { class: "m-0 text-[15px] text-ctp-subtext1", "Enter the code the device shows. It will be signed in as " b { "{user.username}" } "." }
            }
            Field { label: "Code", value: code, autocomplete: "one-time-code", placeholder: "BCDF-GHJK", tall: true }
            button { class: "btn-primary", r#type: "submit", "Continue" }
        }
    }
}

async fn lookup(code: String) -> Result<DeviceSignIn, ApiError> {
    let client = rpc::client().await?;
    client.device_sign_in_lookup(code).await.map_err(rpc::error)
}

/// What became of the request on this page.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Approved,
    Denied,
}

/// "Sign in to Spoolframe on “<device>”?", with Cancel and Sign in.
#[component]
fn Consent(user: UserInfo, code: String, server: String, on_switch: EventHandler<()>) -> Element {
    let shown = display_code(&code);
    let asked = code.clone();
    let device = use_resource(move || lookup(asked.clone()));
    let mut outcome = use_signal(|| None::<Outcome>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let who = user.display_name.clone().unwrap_or(user.username.clone());
    let decide = use_callback(move |decision: Outcome| {
        let code = code.clone();
        busy.set(true);
        spawn(async move {
            let decided = async {
                let client = rpc::client().await?;
                match decision {
                    // Never remembered until this page shows the device's
                    // request and lets the approver untick it (P4-03).
                    Outcome::Approved => client.device_sign_in_approve(code, false).await,
                    Outcome::Denied => client.device_sign_in_deny(code).await,
                }
                .map_err(rpc::error)
            };
            match decided.await {
                Ok(()) => outcome.set(Some(decision)),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    // "Not you?": sign out here, and the sign-in card comes back on the
    // same code.
    let switch = move |_| {
        spawn(async move {
            let _ = http::logout().await;
            rpc::forget();
            on_switch.call(());
        });
    };
    let current = device.read().clone();
    rsx! {
        div { class: "glass-card glass-card-wide",
            match current {
                None => rsx! { p { class: "m-0 text-center text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! {
                    div { class: "flex flex-col items-center gap-3 text-center",
                        icons::Mark { size: 52 }
                        h1 { class: "m-0 text-2xl font-bold", "Sign in a device" }
                    }
                    Alert { text: e.explain() }
                    Link { class: "btn-secondary", to: Route::DeviceApproval { code: String::new() }, "Enter another code" }
                },
                Some(Ok(device)) => {
                    let name = device.device_name.clone().unwrap_or_else(|| "a device".to_owned());
                    match outcome() {
                        Some(Outcome::Approved) => rsx! {
                            div { class: "flex flex-col items-center gap-3 text-center",
                                icons::Mark { size: 52 }
                                h1 { class: "m-0 text-2xl font-bold text-ctp-green", "Signed in" }
                                p { class: "m-0 text-[15px] text-ctp-subtext1", "{name} is now signed in as " b { "{who}" } ". You can close this page." }
                            }
                            Link { class: "btn-secondary", to: Route::Start {}, "Open the web UI" }
                        },
                        Some(Outcome::Denied) => rsx! {
                            div { class: "flex flex-col items-center gap-3 text-center",
                                icons::Mark { size: 52 }
                                h1 { class: "m-0 text-2xl font-bold", "Not signed in" }
                                p { class: "m-0 text-[15px] text-ctp-subtext1", "{name} was refused. Nobody is signed in there." }
                            }
                            Link { class: "btn-secondary", to: Route::Start {}, "Open the web UI" }
                        },
                        None => rsx! {
                            div { class: "flex flex-col items-center gap-3 text-center",
                                icons::Mark { size: 52 }
                                span { class: "text-ctp-teal", icons::Monitor {} }
                                h1 { class: "m-0 break-words text-2xl font-bold", "Sign in to Spoolframe on “{name}”?" }
                                // Which kind of device asks, as it said: a
                                // code from a TV typed on a phone should
                                // say "Apple TV app".
                                p { class: "m-0 text-[15px] font-semibold text-ctp-teal", "{device.platform.label()}" }
                                p { class: "m-0 text-[15px] text-ctp-subtext1",
                                    "You are signed in as " b { "{who}" } " ({user.username}) on {server}."
                                }
                                p { class: "m-0 text-[13px] text-ctp-subtext0",
                                    "{name} gets its own session. You can sign it out from My profile later."
                                }
                                p { class: "mono m-0 text-[13px] text-ctp-subtext0",
                                    "{device.client_name} · code {shown}"
                                }
                            }
                            if let Some(text) = error() {
                                Alert { text }
                            }
                            div { class: "grid grid-cols-2 gap-2.5",
                                button { class: "btn-secondary", disabled: busy(), onclick: move |_| decide.call(Outcome::Denied), "Cancel" }
                                button { class: "btn-primary", disabled: busy(), onclick: move |_| decide.call(Outcome::Approved), "Sign in" }
                            }
                            button { class: "btn-plain text-center text-[13px] text-ctp-teal", r#type: "button", onclick: switch,
                                "Not you? Use another account"
                            }
                        },
                    }
                },
            }
        }
    }
}
