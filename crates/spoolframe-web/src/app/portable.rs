//! The portable identity card (P7-04): the badge a tied account carries,
//! on a user's page in Users and in My profile, with Sever. Tying is the
//! identity's phone app's: the web UI never holds the root, so it shows
//! and severs only. Severing long after signing in asks "Confirm it's
//! you" first, then tries again.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, MeApiClient, PortableTie};

use super::admin::{CopyButton, Said, say};
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{icons, rpc};
use crate::logic::ApiError;
use crate::portable::{
    CONFIRM_ABOUT, Owner, badge_title, sever_question, severed, tie_line, tied_note, untied_line,
};

/// Sever the tie `owner` names: the caller's own, or `username`'s.
async fn sever(owner: &Owner, username: &str) -> Result<(), ApiError> {
    let client = rpc::client().await?;
    match owner {
        Owner::Me => client.portable_untie().await.map_err(rpc::error),
        Owner::User { .. } => client
            .user_untie(username.to_owned())
            .await
            .map_err(rpc::error),
    }
}

/// `tie`, if the account has one, as a badge with Sever; `username` is the
/// account's, for an admin's sever. `on_severed` hears once it is gone.
#[component]
pub(super) fn PortableCard(
    owner: Owner,
    username: String,
    tie: Option<PortableTie>,
    toast: Signal<Option<Said>>,
    on_severed: EventHandler<()>,
) -> Element {
    let mut asking = use_signal(|| false);
    let mut busy = use_signal(|| false);
    // Severing may need "Confirm it's you" first, then is tried again.
    let confirming = use_signal(|| None::<String>);
    let go = {
        let owner = owner.clone();
        let username = username.clone();
        use_callback(move |()| {
            let owner = owner.clone();
            let username = username.clone();
            busy.set(true);
            spawn(async move {
                match sever(&owner, &username).await {
                    Ok(()) => {
                        say(toast, true, severed(&owner));
                        on_severed.call(());
                    }
                    // Kept asking, to try again once confirmed.
                    Err(e) if asks_to_confirm(&e, confirming, CONFIRM_ABOUT) => {
                        busy.set(false);
                        return;
                    }
                    Err(e) => say(toast, false, e.explain()),
                }
                busy.set(false);
                asking.set(false);
            });
        })
    };
    let (question, consequence) = sever_question(&owner);
    let head = rsx! {
        icons::Fingerprint { size: 18 }
        h2 { "Portable identity" }
    };
    let body = match &tie {
        None => rsx! {
            p { class: "m-0 text-sm text-ctp-subtext0", {untied_line(&owner)} }
        },
        Some(tie) => rsx! {
            div { class: "row",
                span { class: "flex min-w-0 items-center gap-3",
                    span { class: "flex text-ctp-teal", icons::ShieldCheck { size: 18 } }
                    span { class: "row-what",
                        b {
                            span { class: "pill pill-admin font-mono", title: badge_title(tie), "{tie.fingerprint}" }
                        }
                        span { class: "row-sub", {tie_line(tie)} }
                    }
                }
                span { class: "row-do",
                    CopyButton { text: tie.id.clone(), label: "Copy id" }
                    if !asking() {
                        button { class: "btn btn-danger", onclick: move |_| asking.set(true),
                            icons::Unlink { size: 15 }
                            "Sever"
                        }
                    }
                }
            }
            if asking() {
                div { class: "flex flex-col gap-2",
                    b { class: "text-sm", "{question}" }
                    p { class: "m-0 text-[13px] text-ctp-subtext0", "{consequence}" }
                    span { class: "flex gap-2",
                        button { class: "btn btn-danger", disabled: busy(), onclick: move |_| go.call(()), "Sever" }
                        button { class: "btn", disabled: busy(), onclick: move |_| asking.set(false), "Keep" }
                    }
                }
            }
            p { class: "m-0 text-xs text-ctp-subtext0", {tied_note(&owner)} }
        },
    };
    rsx! {
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| go.call(()) }
        match owner {
            Owner::Me => rsx! {
                section { class: "card",
                    div { class: "card-head", {head} }
                    div { class: "card-body", {body} }
                }
            },
            Owner::User { name } => rsx! {
                div { class: "section-head",
                    h2 { "Portable identity" }
                    span { "The identity {name}'s account is tied to, if any." }
                }
                section { class: "card",
                    div { class: "card-body", {body} }
                }
            },
        }
    }
}
