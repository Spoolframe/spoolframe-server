//! The portable identity section (P7-04, P7-09; canvas v109 WebTies): the
//! identities a local account is tied to, on a user's page in Users and in
//! My profile, each with its fingerprint, when it was tied and last used,
//! and Sever. Tying is the identity's app's: the web UI never holds the
//! root, so it shows and severs only. Severing long after signing in asks
//! "Confirm it's you" first, then tries again.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, MeApiClient, PortableTie};

use super::admin::{Said, say};
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{icons, now, rpc};
use crate::logic::ApiError;
use crate::portable::{
    CONFIRM_ABOUT, Owner, badge_title, heading_line, sever_question, severed, tie_line, untied_note,
};

/// Sever the tie to identity `id` that `owner` names: the caller's own, or
/// `username`'s.
async fn sever(owner: &Owner, username: &str, id: &str) -> Result<(), ApiError> {
    let client = rpc::client().await?;
    match owner {
        Owner::Me => client
            .portable_untie(id.to_owned())
            .await
            .map_err(rpc::error),
        Owner::User { .. } => client
            .user_untie(username.to_owned(), id.to_owned())
            .await
            .map_err(rpc::error),
    }
}

/// The account's `ties`, the oldest first, each with Sever; `username` is
/// the account's, for an admin's sever. `on_severed` hears the identity's
/// id once its tie is gone.
#[component]
pub(super) fn PortableCard(
    owner: Owner,
    username: String,
    ties: Vec<PortableTie>,
    toast: Signal<Option<Said>>,
    on_severed: EventHandler<String>,
) -> Element {
    // The tie whose Sever was pressed: the question is asked about it.
    let mut asking = use_signal(|| None::<PortableTie>);
    let mut busy = use_signal(|| false);
    // Severing may need "Confirm it's you" first, then is tried again.
    let confirming = use_signal(|| None::<String>);
    let go = {
        let owner = owner.clone();
        let username = username.clone();
        use_callback(move |()| {
            let Some(tie) = asking() else {
                return;
            };
            let owner = owner.clone();
            let username = username.clone();
            busy.set(true);
            spawn(async move {
                match sever(&owner, &username, &tie.id).await {
                    Ok(()) => {
                        say(toast, true, severed(&tie));
                        on_severed.call(tie.id.clone());
                    }
                    // Kept asking, to try again once confirmed.
                    Err(e) if asks_to_confirm(&e, confirming, CONFIRM_ABOUT) => {
                        busy.set(false);
                        return;
                    }
                    Err(e) => say(toast, false, e.explain()),
                }
                busy.set(false);
                asking.set(None);
            });
        })
    };
    let others = ties.len().saturating_sub(1);
    let (question, before, after) = sever_question(&owner, others);
    let seen = now();
    rsx! {
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| go.call(()) }
        div { class: "flex flex-col gap-2.5",
            div { class: "section-head",
                h2 { "Portable identity" }
                span { {heading_line(&owner, ties.len())} }
            }
            if ties.is_empty() {
                section { class: "card",
                    div { class: "card-body",
                        div { class: "flex items-center gap-3",
                            span { class: "tie-icon tie-icon-empty", icons::Fingerprint { size: 22 } }
                            span { class: "row-what",
                                b { "Portable identity" }
                                span { class: "tie-sub", "Not tied" }
                            }
                        }
                        p { class: "m-0 text-[13px] text-ctp-subtext1", {untied_note(&owner)} }
                    }
                }
            } else {
                div { class: "tie-list",
                    for tie in ties {
                        div { class: "tie-row", key: "{tie.id}",
                            span { class: "tie-icon", icons::Fingerprint { size: 20 } }
                            span { class: "tie-what",
                                span { class: "tie-fp", title: badge_title(&tie), "{tie.fingerprint}" }
                                span { class: "tie-sub", {tie_line(seen, &tie)} }
                            }
                            button {
                                class: "btn btn-sever",
                                onclick: {
                                    let tie = tie.clone();
                                    move |_| asking.set(Some(tie.clone()))
                                },
                                icons::Unlink { size: 14 }
                                "Sever…"
                            }
                        }
                    }
                }
            }
        }
        if let Some(tie) = asking() {
            div { class: "scrim",
                section { class: "dialog w-[520px]", role: "dialog",
                    div { class: "dialog-head dialog-head-danger",
                        icons::Unlink { size: 22 }
                        h1 { "{question}" }
                    }
                    p {
                        "{before}"
                        span { class: "tie-fp-inline", "{tie.fingerprint}" }
                        "{after}"
                    }
                    div { class: "dialog-actions",
                        button {
                            class: "btn-dialog btn-dialog-outline",
                            disabled: busy(),
                            onclick: move |_| asking.set(None),
                            "Cancel"
                        }
                        button {
                            class: "btn-dialog btn-dialog-danger",
                            disabled: busy(),
                            onclick: move |_| go.call(()),
                            icons::Unlink { size: 16 }
                            "Sever"
                        }
                    }
                }
            }
        }
    }
}
