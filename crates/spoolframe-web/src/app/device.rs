//! Approving a device's sign-in from a browser (P3-07, P3-05, P4-03): the
//! page a device's QR code opens, `/auth/device?code=…`, in each state the
//! WebApproveStates board draws. An app cannot use a passkey itself; a
//! phone's browser can, here, and then says yes for the app.

use dioxus::prelude::*;
use spoolframe_admin_api::{DeviceSignIn, MeApiClient, UserInfo};

use super::admin::sleep;
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{Alert, Centered, Notice, Route, SignInForm, http, icons, now_ms, rpc};
use crate::approve::{self, Refused};
use crate::logic::{ApiError, display_code};
use crate::trust;

/// Where the page is.
const PATH: &str = "/auth/device";

/// `text` as a query value.
fn encode(text: &str) -> String {
    js_sys::encode_uri_component(text).into()
}

/// The host this page is open at, as the fingerprint box names it.
fn page_host() -> String {
    web_sys::window()
        .and_then(|w| w.location().host().ok())
        .unwrap_or_default()
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
    let has_code = !code.trim().is_empty();
    rsx! {
        Centered {
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(e)) => rsx! { div { class: "glass-card", Alert { text: e.explain() } } },
                // Signed out (WebApproveStates 2): the device's name needs
                // someone signed in to look up, so the card names the code.
                Some(Ok(None)) => rsx! {
                    SignInForm {
                        title: "Sign in to {server}",
                        lede: if has_code { "to approve the device showing this code" } else { "to sign in a device" },
                        code: has_code.then(|| display_code(&code)),
                        submit_label: if has_code { "Sign in and continue" } else { "Sign in" },
                        footer: if has_code { "After signing in you come straight back to this code." } else { "After signing in you type the code the device shows." },
                        public_url,
                        insecure_cookies,
                        sso,
                        said,
                        return_to,
                        on_done: move |_| me.restart(),
                    }
                },
                Some(Ok(Some(_))) if !has_code => rsx! {
                    CodeCard {
                        typed: String::new(),
                        refused: None,
                        on_submit: move |typed: String| {
                            navigator().replace(Route::DeviceApproval { code: typed });
                        },
                    }
                },
                Some(Ok(Some(user))) => rsx! {
                    Consent { key: "{code}", user, code: code.clone(), server, on_switch: move |_| me.restart() }
                },
            }
        }
    }
}

/// The mark and title every card of the page starts with.
#[component]
fn CardHead(title: String, #[props(default)] lede: Option<String>) -> Element {
    rsx! {
        div { class: "approve-head",
            icons::Mark { size: 48 }
            h1 { "{title}" }
            if let Some(lede) = lede {
                p { "{lede}" }
            }
        }
    }
}

/// A code to type (WebApproveStates 3), or typed and refused: unknown,
/// used or expired (4), or one too many (5), with the wait counted down
/// and nothing to press until it is over.
#[component]
fn CodeCard(typed: String, refused: Option<Refused>, on_submit: EventHandler<String>) -> Element {
    let shown = if typed.is_empty() {
        String::new()
    } else {
        display_code(&typed)
    };
    let refused_code = shown.clone();
    let mut code = use_signal(|| shown.clone());
    // The countdown's clock, a second at a time.
    let mut clock = use_signal(now_ms);
    use_future(move || async move {
        loop {
            sleep(1000).await;
            clock.set(now_ms());
        }
    });
    let wait = match &refused {
        Some(Refused::Limited { until_ms }) => approve::countdown(clock(), *until_ms),
        _ => None,
    };
    let locked = wait.is_some();
    let wrong = matches!(refused, Some(Refused::Wrong));
    // Red while it is the code that was refused; teal again once changed.
    let bad = wrong && display_code(&code()) == refused_code;
    let submit = move |e: FormEvent| {
        e.prevent_default();
        if !locked {
            on_submit.call(code());
        }
    };
    rsx! {
        form { class: "glass-card approve-card", onsubmit: submit,
            CardHead {
                title: "Sign in a device",
                lede: refused.is_none().then(|| "Type the code shown on its screen.".to_owned()),
            }
            match (&refused, &wait) {
                (Some(Refused::Wrong), _) => rsx! {
                    Refusal {
                        title: "That code doesn't work",
                        text: "It may have expired (codes last 5 minutes) or been used already. Get a new code on the device.",
                    }
                },
                (Some(Refused::Limited { .. }), Some(left)) => rsx! {
                    Refusal { title: "Too many wrong codes", text: "Try again in {left}." }
                },
                (Some(Refused::Other(text)), _) => rsx! { Alert { text: text.clone() } },
                _ => rsx! {},
            }
            div { class: if locked { "approve-code approve-locked" } else { "approve-code" }, aria_disabled: "{locked}",
                input {
                    class: if bad { "code-field code-field-bad" } else { "code-field" },
                    aria_label: "Code",
                    aria_invalid: bad,
                    autocomplete: "one-time-code",
                    autocapitalize: "characters",
                    spellcheck: "false",
                    placeholder: if locked { "" } else { "BCDF-GHJK" },
                    disabled: locked,
                    value: "{code}",
                    oninput: move |e| code.set(e.value()),
                }
                if refused.is_none() {
                    span { class: "approve-hint", "8 letters. Dashes and lowercase are fine." }
                }
                button {
                    class: "btn-primary btn-tall",
                    r#type: "submit",
                    disabled: locked || code().trim().is_empty(),
                    if wrong { "Try again" } else { "Continue" }
                }
            }
        }
    }
}

/// Why the code was refused: the board's red box, a circled x and two
/// lines.
#[component]
fn Refusal(title: String, text: String) -> Element {
    rsx! {
        div { class: "alert alert-error approve-refusal", role: "alert",
            icons::CircleX {}
            span { class: "flex flex-col gap-[3px]",
                b { "{title}" }
                span { "{text}" }
            }
        }
    }
}

/// A code looked up, or why not and when the answer came.
async fn lookup(code: String) -> Result<DeviceSignIn, Refused> {
    let found = async {
        let client = rpc::client().await?;
        client.device_sign_in_lookup(code).await.map_err(rpc::error)
    };
    found
        .await
        .map_err(|e: ApiError| approve::refused(&e, now_ms()))
}

/// What became of the request on this page.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Approved,
    Denied,
}

/// "Sign in “<device>”?" (WebApproveStates 1 and 6), with Deny and
/// Approve, then what was decided (7 and 8).
#[component]
fn Consent(user: UserInfo, code: String, server: String, on_switch: EventHandler<()>) -> Element {
    let shown = display_code(&code);
    let asked = code.clone();
    let mut device = use_resource(move || lookup(asked.clone()));
    let mut outcome = use_signal(|| None::<Outcome>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // "This device will be remembered", ticked as the page opens: shown
    // only when the device asked, and the approver's answer decides
    // (P4-01).
    let mut ticked = use_signal(|| true);
    // Approving needs a recent sign-in: "Confirm it's you", then again.
    let confirming = use_signal(|| None::<String>);
    let who = user.display_name.clone().unwrap_or(user.username.clone());
    let approving = code.clone();
    let decide = use_callback(move |decision: Outcome| {
        let code = approving.clone();
        let asked = matches!(&*device.peek(), Some(Ok(d)) if d.remember);
        let remember = trust::remembers(asked, ticked());
        busy.set(true);
        spawn(async move {
            let decided = async {
                let client = rpc::client().await?;
                match decision {
                    Outcome::Approved => client.device_sign_in_approve(code, remember).await,
                    Outcome::Denied => client.device_sign_in_deny(code).await,
                }
                .map_err(rpc::error)
            };
            match decided.await {
                Ok(()) => outcome.set(Some(decision)),
                Err(e) if asks_to_confirm(&e, confirming, "signing in a device") => {}
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
    match current {
        None => rsx! {
            div { class: "glass-card approve-card",
                p { class: "m-0 text-center text-ctp-subtext0", "Loading…" }
            }
        },
        Some(Err(refused)) => {
            let looked_up = shown.clone();
            rsx! {
                CodeCard {
                    typed: code.clone(),
                    refused,
                    on_submit: move |typed: String| {
                        if display_code(&typed) == looked_up {
                            device.restart();
                        } else {
                            navigator().replace(Route::DeviceApproval { code: typed });
                        }
                    },
                }
            }
        }
        Some(Ok(found)) => {
            let word = approve::device_word(found.platform);
            let remembered = trust::remembers(found.remember, ticked());
            match outcome() {
                Some(Outcome::Approved) => rsx! {
                    Decided {
                        approved: true,
                        title: match &found.device_name {
                            Some(name) => format!("{name} is signed in"),
                            None => "The device is signed in".to_owned(),
                        },
                        text: approve::approved_line(remembered),
                    }
                },
                Some(Outcome::Denied) => rsx! {
                    Decided {
                        approved: false,
                        title: "Sign-in denied",
                        text: approve::denied_line(found.device_name.as_deref().unwrap_or("The device")),
                    }
                },
                None => rsx! {
                    ConfirmItsYou { asking: confirming, on_confirmed: move |_| decide.call(Outcome::Approved) }
                    section { class: "glass-card approve-card",
                        CardHead {
                            title: match &found.device_name {
                                Some(name) => format!("Sign in “{name}”?"),
                                None => "Sign in this device?".to_owned(),
                            },
                            lede: approve::check_line(word),
                        }
                        div { class: "approve-shown",
                            span { "Code on the {word}" }
                            b { class: "approve-code-shown", "{shown}" }
                        }
                        div { class: "approve-server",
                            span { "{server} · {page_host()}" }
                            b { class: "approve-fingerprint", "{found.server_fingerprint}" }
                        }
                        match approve::origin(
                            found.address.as_deref(),
                            found.from_internet,
                            found.public_ipv6,
                            word,
                        ) {
                            Some((line, true)) => rsx! { Notice { "{line}" } },
                            Some((line, false)) => rsx! { p { class: "approve-who", "{line}" } },
                            None => rsx! {},
                        }
                        p { class: "approve-who",
                            "You're signed in as " b { "{who}" } {approve::signed_in_as_tail(word)}
                        }
                        if let Some(line) = trust::remember_line(found.remember, ticked(), &who, word) {
                            div { class: "remember-box",
                                button {
                                    class: if ticked() { "box box-on" } else { "box" },
                                    r#type: "button",
                                    role: "checkbox",
                                    aria_checked: "{ticked()}",
                                    aria_labelledby: "remember-label",
                                    aria_describedby: "remember-line",
                                    disabled: busy(),
                                    onclick: move |_| ticked.toggle(),
                                    if ticked() { icons::Check {} }
                                }
                                span { class: "flex flex-col gap-0.5",
                                    b { id: "remember-label", "This device will be remembered" }
                                    span { id: "remember-line", class: "remember-line", "{line}" }
                                }
                            }
                        }
                        if let Some(text) = error() {
                            Alert { text }
                        }
                        div { class: "grid grid-cols-2 gap-2.5",
                            button { class: "btn-secondary btn-tall", disabled: busy(), onclick: move |_| decide.call(Outcome::Denied), "Deny" }
                            button { class: "btn-primary btn-tall", disabled: busy(), onclick: move |_| decide.call(Outcome::Approved), "Approve" }
                        }
                        button { class: "btn-plain text-center text-[13px] text-ctp-teal", r#type: "button", onclick: switch,
                            "Not you? Use another account"
                        }
                    }
                },
            }
        }
    }
}

/// What was decided (WebApproveStates 7 and 8): a big green check or a
/// grey circled x, and nothing more to do.
#[component]
fn Decided(approved: bool, title: String, text: String) -> Element {
    rsx! {
        section { class: "glass-card approve-card", role: "status",
            div { class: "approve-done",
                span { class: if approved { "text-ctp-green" } else { "text-ctp-subtext0" },
                    if approved {
                        icons::CircleCheck { size: 56 }
                    } else {
                        icons::CircleX { size: 56 }
                    }
                }
                h1 { "{title}" }
                p { "{text}" }
            }
        }
    }
}
