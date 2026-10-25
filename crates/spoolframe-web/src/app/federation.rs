//! Users and devices › Federation (P8-01, experimental): the servers
//! paired with this one, each with its key's fingerprint to compare with
//! what that server's admin sees, and Unpair; a paired server's new key,
//! held until it is confirmed or rejected; inviting a server (a
//! `federation` invite, shown as Invites shows one) and pairing with
//! another by its invite link, after checking its fingerprint, then
//! choosing what to share with it. Each live pair says whether it is
//! online, when what it shares was last synced, with Refresh (P8-02), and
//! its plays, one number each way and no user named; and lists the
//! libraries shared with it, with Share and Revoke (P8-03). Each pair's
//! name opens its own page: what is shared each way, the week's plays,
//! what state it is in, the new key its admin said to expect (M8-06),
//! and Unpair (the WebFederationPeer board). Every
//! change asks "Confirm it's you" when the sign-in is old.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, FederationApiClient, GrantEntry, InviteCreated, InviteKind, LibraryEntry,
    NewInvite, PairLookup, PeerDetail, PeerEntry, PeerKey,
};

use super::admin::{Said, Toast, say};
use super::invites::InviteResult;
use super::optimized::DangerDialog;
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{Alert, Field, Route, icons, now, rpc};
use crate::federation::{
    CONFIRM_ABOUT, CONFIRM_EXPECT, CONFIRM_SHARE, INVITE_SPENT, ask_to_compare, expected_line,
    freshness, from_line, is_spent, key_change_error, new_key_line, paired_line, pairing_means,
    peer_line, presence, received_line, refusal, revoke_question, shared, state_line, states,
    unpair_means, unpair_question, unpaired, week,
};
use crate::libraries::items_line;
use crate::logic::ApiError;

async fn peers() -> Result<Vec<PeerEntry>, ApiError> {
    let client = rpc::client().await?;
    client.peer_list().await.map_err(rpc::error)
}

/// A change to the pair `id`, as the page asks for it.
#[derive(Debug, Clone, PartialEq)]
enum Change {
    Unpair(PeerEntry),
    /// A key held for the pair, named as the page listed it.
    Confirm(PeerEntry, PeerKey),
    Reject(PeerEntry, PeerKey),
}

async fn change(change: &Change) -> Result<String, ApiError> {
    let client = rpc::client().await?;
    match change {
        Change::Unpair(peer) => {
            client
                .peer_revoke(peer.id.clone())
                .await
                .map_err(rpc::error)?;
            Ok(unpaired(peer))
        }
        Change::Confirm(peer, key) => {
            client
                .key_confirm(peer.id.clone(), key.endpoint_id.clone())
                .await
                .map_err(rpc::error)?;
            Ok(format!("{} moved to its new key", peer.name))
        }
        Change::Reject(peer, key) => {
            client
                .key_reject(peer.id.clone(), key.endpoint_id.clone())
                .await
                .map_err(rpc::error)?;
            Ok(format!("{}'s new key rejected", peer.name))
        }
    }
}

#[component]
pub fn Federation() -> Element {
    let mut list = use_resource(peers);
    let toast = use_signal(|| None::<Said>);
    let mut pairing = use_signal(|| false);
    let mut made = use_signal(|| None::<InviteCreated>);
    let mut asking = use_signal(|| None::<Change>);
    let mut busy = use_signal(|| false);
    // Each change may ask "Confirm it's you" first, then is tried again.
    let confirming = use_signal(|| None::<String>);
    let confirming_invite = use_signal(|| None::<String>);
    // "Invite a server" makes a `federation` invite, as Invites makes one.
    let invite = use_callback(move |()| {
        spawn(async move {
            let made_now = async {
                let client = rpc::client().await?;
                client
                    .invite_create(NewInvite {
                        kind: InviteKind::Federation,
                        note: None,
                        uses: None,
                        expires: None,
                        role: None,
                    })
                    .await
                    .map_err(rpc::error)
            }
            .await;
            match made_now {
                Ok(created) => made.set(Some(created)),
                Err(e) if asks_to_confirm(&e, confirming_invite, CONFIRM_ABOUT) => {}
                Err(e) => say(toast, false, e.explain()),
            }
        });
    });
    let go = use_callback(move |()| {
        let Some(asked) = asking() else {
            return;
        };
        busy.set(true);
        spawn(async move {
            match change(&asked).await {
                Ok(text) => {
                    say(toast, true, text);
                    list.restart();
                }
                // Kept asking, to try again once confirmed.
                Err(e) if asks_to_confirm(&e, confirming, CONFIRM_ABOUT) => {
                    busy.set(false);
                    return;
                }
                Err(e) => {
                    say(toast, false, key_change_error(&e));
                    // What the page shows may be what changed.
                    list.restart();
                }
            }
            busy.set(false);
            asking.set(None);
        });
    });
    let now = now();
    let current = list.read().clone();
    rsx! {
        div { class: "flex flex-col gap-6",
            div { class: "page-head",
                div { class: "flex flex-col gap-1.5",
                    h1 { class: "page-title page-title-admin hidden md:block", "Federation" }
                    span { class: "hidden max-w-[680px] text-sm leading-normal text-ctp-subtext0 md:inline",
                        "Servers paired with this one. Experimental: pair only with servers you run. Compare each key's fingerprint with what its admin sees as their own."
                    }
                }
                div { class: "flex gap-2",
                    button { class: "btn", onclick: move |_| invite.call(()),
                        icons::Ticket { size: 16 }
                        "Invite a server"
                    }
                    button { class: "btn-add", onclick: move |_| pairing.set(true),
                        icons::LinkChain { size: 16 }
                        "Pair with a server"
                    }
                }
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(error)) => rsx! { Alert { text: error.explain() } },
                Some(Ok(peers)) if peers.is_empty() => rsx! {
                    section { class: "card empty",
                        icons::Server { size: 22 }
                        b { "No paired servers" }
                        span { "Invite a server, or paste another server's invite link to pair with it." }
                    }
                },
                Some(Ok(peers)) => rsx! {
                    div { class: "tie-list",
                        for peer in peers {
                            PeerRow {
                                key: "{peer.id}",
                                peer: peer.clone(),
                                now,
                                toast,
                                on_change: move |c: Change| asking.set(Some(c)),
                                on_refreshed: move |_| list.restart(),
                            }
                        }
                    }
                },
            }
        }
        if pairing() {
            PairDialog {
                on_done: move |paired: Option<PeerEntry>| {
                    pairing.set(false);
                    if let Some(peer) = paired {
                        say(toast, true, format!("Paired with {}", peer.name));
                        list.restart();
                    }
                },
            }
        }
        if let Some(created) = made() {
            InviteResult { created, on_done: move |_| made.set(None) }
        }
        if let Some(asked) = asking() {
            ChangeDialog { change: asked, busy: busy(), on_go: move |_| go.call(()), on_cancel: move |_| asking.set(None) }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| go.call(()) }
        ConfirmItsYou { asking: confirming_invite, on_confirmed: move |_| invite.call(()) }
        Toast { said: toast }
    }
}

/// One pair: its name, fingerprint and line, Unpair while paired, and
/// each new key held for it, with its fingerprint, Confirm and Reject.
/// While paired: its state and Refresh, and what is shared with it.
#[component]
fn PeerRow(
    peer: PeerEntry,
    now: i64,
    toast: Signal<Option<Said>>,
    on_change: EventHandler<Change>,
    on_refreshed: EventHandler<()>,
) -> Element {
    let live = peer.revoked_at.is_none();
    let mut refreshing = use_signal(|| false);
    let refresh = {
        let peer = peer.clone();
        move |_| {
            let peer = peer.clone();
            refreshing.set(true);
            spawn(async move {
                let synced = async {
                    let client = rpc::client().await?;
                    client
                        .peer_refresh(peer.id.clone())
                        .await
                        .map_err(rpc::error)
                }
                .await;
                match synced {
                    Ok(()) => say(toast, true, format!("Synced {}", peer.name)),
                    Err(e) => say(toast, false, e.explain()),
                }
                refreshing.set(false);
                on_refreshed.call(());
            });
        }
    };
    rsx! {
        div { class: "tie-row",
            span { class: "tie-icon", icons::Server { size: 20 } }
            span { class: "tie-what",
                Link { class: "font-bold hover:underline", to: Route::FederationPeer { id: peer.id.clone() }, "{peer.name}" }
                span { class: "tie-fp", title: "Key {peer.endpoint_id}", "{peer.fingerprint}" }
                span { class: "tie-sub", {peer_line(now, &peer)} }
                if live {
                    span { class: "tie-sub", {state_line(now, &peer)} }
                    Shares { peer: peer.clone(), toast }
                }
                for key in peer.held_keys.clone() {
                    span { class: "tie-sub", {new_key_line(now, &key)} }
                    span { class: "tie-fp", title: "Key {key.endpoint_id}", "{key.fingerprint}" }
                    if live && key.rejected_at.is_none() {
                        span { class: "flex gap-2",
                            button {
                                class: "btn",
                                onclick: {
                                    let (peer, key) = (peer.clone(), key.clone());
                                    move |_| on_change.call(Change::Confirm(peer.clone(), key.clone()))
                                },
                                icons::Check { size: 14 }
                                "Confirm key"
                            }
                            button {
                                class: "btn",
                                onclick: {
                                    let (peer, key) = (peer.clone(), key.clone());
                                    move |_| on_change.call(Change::Reject(peer.clone(), key.clone()))
                                },
                                icons::X { size: 14 }
                                "Reject key"
                            }
                        }
                    }
                }
            }
            if live {
                span { class: "flex gap-2",
                    button { class: "btn", disabled: refreshing(), onclick: refresh,
                        "Refresh"
                    }
                    button {
                        class: "btn btn-sever",
                        onclick: {
                            let peer = peer.clone();
                            move |_| on_change.call(Change::Unpair(peer.clone()))
                        },
                        icons::Unlink { size: 14 }
                        "Unpair…"
                    }
                }
            }
        }
    }
}

/// The libraries shared with `peer`, and the server's own that are not.
async fn shares_of(peer: String) -> Result<(Vec<GrantEntry>, Vec<LibraryEntry>), ApiError> {
    let client = rpc::client().await?;
    let grants = client.peer_grant_list(peer).await.map_err(rpc::error)?;
    let libraries = client.libraries().await.map_err(rpc::error)?.libraries;
    Ok((grants, libraries))
}

/// A share asked for: a library to share with the pair, or to stop
/// sharing.
#[derive(Debug, Clone, PartialEq)]
enum Share {
    Add(String),
    Revoke(String),
}

/// Make `share` with pair `peer`.
async fn apply_share(peer: &str, share: &Share) -> Result<(), ApiError> {
    let client = rpc::client().await?;
    match share {
        Share::Add(library) => client
            .peer_grant_add(peer.to_owned(), library.clone())
            .await
            .map(|_| ())
            .map_err(rpc::error),
        Share::Revoke(library) => client
            .peer_grant_revoke(peer.to_owned(), library.clone())
            .await
            .map_err(rpc::error),
    }
}

/// What is shared with `peer` (P8-03): each library with Revoke, and
/// Share for one of the server's own that is not. Every user of the
/// paired server may browse and watch what is shared; there is nothing
/// per user.
#[component]
fn Shares(peer: PeerEntry, toast: Signal<Option<Said>>) -> Element {
    let id = peer.id.clone();
    let mut listed = use_resource(move || shares_of(id.clone()));
    let mut chosen = use_signal(String::new);
    let mut asked = use_signal(|| None::<Share>);
    let confirming = use_signal(|| None::<String>);
    // The library to stop sharing, by id and name, while asked to confirm
    // (as Unpair is).
    let mut revoking = use_signal(|| None::<(String, String)>);
    let mut busy = use_signal(|| false);
    let go = {
        let peer = peer.clone();
        use_callback(move |()| {
            let Some(share) = asked() else {
                return;
            };
            let peer = peer.clone();
            busy.set(true);
            spawn(async move {
                match apply_share(&peer.id, &share).await {
                    Ok(()) => {
                        let (library, now_shared) = match &share {
                            Share::Add(l) => (l, true),
                            Share::Revoke(l) => (l, false),
                        };
                        say(toast, true, shared(&peer, library, now_shared));
                    }
                    // Kept asked, to try again once confirmed.
                    Err(e) if asks_to_confirm(&e, confirming, CONFIRM_SHARE) => {
                        busy.set(false);
                        return;
                    }
                    Err(e) => say(toast, false, e.explain()),
                }
                busy.set(false);
                asked.set(None);
                revoking.set(None);
                chosen.set(String::new());
                listed.restart();
            });
        })
    };
    let current = listed.read().clone();
    rsx! {
        match current {
            None => rsx! { span { class: "tie-sub", "Loading what is shared…" } },
            Some(Err(error)) => rsx! { span { class: "tie-sub", {error.explain()} } },
            Some(Ok((grants, libraries))) => {
                let name = |id: &str| {
                    libraries
                        .iter()
                        .find(|l| l.id == id)
                        .map_or_else(|| id.to_owned(), |l| l.name.clone())
                };
                let unshared: Vec<LibraryEntry> = libraries
                    .iter()
                    .filter(|l| !grants.iter().any(|g| g.library == l.id))
                    .cloned()
                    .collect();
                rsx! {
                    if grants.is_empty() {
                        span { class: "tie-sub", "Nothing shared with this server." }
                    }
                    for grant in grants.clone() {
                        span { key: "{grant.library}", class: "flex items-center gap-2",
                            span { class: "tie-sub", "Shared: " {name(&grant.library)} }
                            button {
                                class: "btn btn-sever",
                                onclick: {
                                    let named = (grant.library.clone(), name(&grant.library));
                                    move |_| revoking.set(Some(named.clone()))
                                },
                                "Revoke…"
                            }
                        }
                    }
                    if !unshared.is_empty() {
                        span { class: "flex items-center gap-2",
                            select {
                                class: "pref-select",
                                onchange: move |e| chosen.set(e.value()),
                                option { value: "", selected: chosen().is_empty(), "Choose a library…" }
                                for library in unshared {
                                    option {
                                        key: "{library.id}",
                                        value: "{library.id}",
                                        selected: library.id == chosen(),
                                        "{library.name}"
                                    }
                                }
                            }
                            button {
                                class: "btn",
                                disabled: chosen().is_empty(),
                                onclick: move |_| {
                                    asked.set(Some(Share::Add(chosen())));
                                    go.call(());
                                },
                                "Share"
                            }
                        }
                    }
                }
            }
        }
        if let Some((library, named)) = revoking() {
            DangerDialog {
                title: revoke_question(&peer, &named).0,
                text: revoke_question(&peer, &named).1,
                icon: rsx! { icons::Server { size: 22 } },
                cancel: "Cancel",
                confirm: "Stop sharing",
                busy: busy(),
                on_cancel: move |()| revoking.set(None),
                on_confirm: move |()| {
                    asked.set(Some(Share::Revoke(library.clone())));
                    go.call(());
                },
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| go.call(()) }
    }
}

/// The question before a change: unpairing, or a held key's fate.
#[component]
fn ChangeDialog(
    change: Change,
    busy: bool,
    on_go: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let (title, text, confirm, danger) = match &change {
        Change::Unpair(peer) => {
            let (title, text) = unpair_question(peer, None);
            (title, text, "Unpair", true)
        }
        Change::Confirm(peer, key) => (
            format!("Move {} to the key {}?", peer.name, key.fingerprint),
            "The pair, and all that hangs on it, moves to this key; the old key, and any other new key held, stop being this server's peer.".to_owned(),
            "Confirm key",
            false,
        ),
        Change::Reject(peer, key) => (
            format!("Reject the key {} for {}?", key.fingerprint, peer.name),
            "The key reaches nothing, and is refused for a day if it says hello again.".to_owned(),
            "Reject key",
            true,
        ),
    };
    rsx! {
        DangerDialog {
            title,
            text,
            warning: !danger,
            icon: rsx! { icons::Server { size: 22 } },
            cancel: "Cancel",
            confirm,
            busy,
            on_cancel: move |()| on_cancel.call(()),
            on_confirm: move |()| on_go.call(()),
        }
    }
}

/// Where pairing is: the link pasted, what it named checked, then what
/// to share with the server paired (the WebFederationPair board).
#[derive(Debug, Clone, PartialEq)]
enum PairStep {
    Paste,
    Review(PairLookup),
    Share(PeerEntry),
}

/// Paste another server's invite link; check its name and fingerprint
/// with its admin; pair; and choose what to share with it, or skip.
#[component]
fn PairDialog(on_done: EventHandler<Option<PeerEntry>>) -> Element {
    let link = use_signal(String::new);
    let mut step = use_signal(|| PairStep::Paste);
    // Why the link was refused: a title, when it is a spent invite, and
    // what to do.
    let mut error = use_signal(|| None::<(Option<&'static str>, String)>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let mut failed = move |e: ApiError| {
        let title = is_spent(&e).then_some("Invite can't be redeemed");
        let text = if title.is_some() {
            INVITE_SPENT.to_owned()
        } else {
            refusal(&e)
        };
        error.set(Some((title, text)));
    };
    // Look the link up, or redeem it once checked: the step says which.
    let attempt = use_callback(move |()| {
        let typed = link().trim().to_owned();
        if typed.is_empty() {
            error.set(Some((None, "Paste the other server's invite link.".into())));
            return;
        }
        let at = step();
        busy.set(true);
        error.set(None);
        spawn(async move {
            let client = rpc::client().await;
            match at {
                PairStep::Paste => {
                    let found = match client {
                        Ok(c) => c.look_up(typed).await.map_err(rpc::error),
                        Err(e) => Err(e),
                    };
                    match found {
                        Ok(found) => step.set(PairStep::Review(found)),
                        Err(e) if asks_to_confirm(&e, confirming, CONFIRM_ABOUT) => {}
                        Err(e) => failed(e),
                    }
                }
                PairStep::Review(_) => {
                    let paired = match client {
                        Ok(c) => c.redeem(typed).await.map_err(rpc::error),
                        Err(e) => Err(e),
                    };
                    match paired {
                        Ok(peer) => step.set(PairStep::Share(peer)),
                        Err(e) if asks_to_confirm(&e, confirming, CONFIRM_ABOUT) => {}
                        Err(e) => {
                            failed(e);
                            step.set(PairStep::Paste);
                        }
                    }
                }
                PairStep::Share(_) => {}
            }
            busy.set(false);
        });
    });
    rsx! {
        match step() {
            PairStep::Paste => rsx! {
                div { class: "scrim",
                    form {
                        class: "dialog w-[520px]",
                        role: "dialog",
                        onsubmit: move |e| {
                            e.prevent_default();
                            attempt.call(());
                        },
                        div { class: "dialog-head",
                            icons::LinkChain { size: 22 }
                            h1 { "Pair with a server" }
                        }
                        p { "Paste the invite link its admin made on their Federation page. This server looks it up first: nothing is paired until you check its fingerprint." }
                        Field { label: "Invite link", value: link, placeholder: "https://spoolframe.app/i#server=…" }
                        if let Some((title, text)) = error() {
                            if let Some(title) = title {
                                div { class: "alert alert-error", role: "alert",
                                    icons::Warning {}
                                    span { class: "flex flex-col gap-0.5", b { "{title}" } span { "{text}" } }
                                }
                            } else {
                                Alert { text }
                            }
                        }
                        div { class: "dialog-actions",
                            button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                            button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(), "Continue" }
                        }
                    }
                }
            },
            PairStep::Review(found) => rsx! {
                div { class: "scrim",
                    section { class: "dialog w-[520px]", role: "dialog",
                        div { class: "dialog-head",
                            icons::Server { size: 22 }
                            h1 { "Pair with {found.name}?" }
                        }
                        div { class: "fingerprint-box",
                            span { "{found.name}'s fingerprint" }
                            div { class: "fingerprint-big", "{found.fingerprint}" }
                            span { {ask_to_compare(&found)} }
                        }
                        ul { class: "flex list-disc flex-col gap-1.5 pl-5 text-sm text-ctp-subtext1",
                            for line in pairing_means(&found) {
                                li { "{line}" }
                            }
                        }
                        div { class: "dialog-actions",
                            button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                            button { class: "btn-dialog btn-dialog-primary", r#type: "button", disabled: busy(), onclick: move |_| attempt.call(()), "Pair" }
                        }
                    }
                }
            },
            PairStep::Share(peer) => rsx! {
                FirstShares { peer, on_done: move |peer| on_done.call(Some(peer)) }
            },
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| attempt.call(()) }
    }
}

/// Just paired with `peer`: which of this server's libraries to share
/// with it (WebFederationPair step 4). Save shares those ticked; Skip
/// shares nothing, as the page's Share does later.
#[component]
fn FirstShares(peer: PeerEntry, on_done: EventHandler<PeerEntry>) -> Element {
    let libraries = use_resource(|| async {
        let client = rpc::client().await?;
        client
            .libraries()
            .await
            .map(|o| o.libraries)
            .map_err(rpc::error)
    });
    let mut ticked = use_signal(Vec::<String>::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let confirming = use_signal(|| None::<String>);
    let save = {
        let peer = peer.clone();
        use_callback(move |()| {
            let peer = peer.clone();
            busy.set(true);
            error.set(None);
            spawn(async move {
                // One at a time: each shared stays shared if a later one
                // fails, and is not shared twice when tried again.
                for library in ticked() {
                    match apply_share(&peer.id, &Share::Add(library.clone())).await {
                        Ok(()) => ticked.with_mut(|t| t.retain(|l| *l != library)),
                        Err(e) if asks_to_confirm(&e, confirming, CONFIRM_SHARE) => {
                            busy.set(false);
                            return;
                        }
                        Err(e) => {
                            error.set(Some(e.explain()));
                            busy.set(false);
                            return;
                        }
                    }
                }
                busy.set(false);
                on_done.call(peer);
            });
        })
    };
    let listed = libraries.read().clone();
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[520px]", role: "dialog",
                div { class: "dialog-head",
                    icons::Check { size: 22 }
                    h1 { "Paired with {peer.name}" }
                }
                p { "Choose what to share with them. Their libraries appear in your users' apps once {peer.name}'s admin shares some." }
                match listed {
                    None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                    Some(Err(e)) => rsx! { Alert { text: e.explain() } },
                    Some(Ok(libraries)) => rsx! {
                        div { class: "flex flex-col gap-3",
                            for library in libraries {
                                {
                                    let on = ticked().contains(&library.id);
                                    let id = library.id.clone();
                                    rsx! {
                                        div { key: "{library.id}", class: "remember-box",
                                            button {
                                                class: if on { "box box-on" } else { "box" },
                                                r#type: "button",
                                                role: "checkbox",
                                                aria_checked: "{on}",
                                                aria_label: "Share {library.name}",
                                                onclick: move |_| {
                                                    let id = id.clone();
                                                    ticked.with_mut(|t| {
                                                        if t.contains(&id) {
                                                            t.retain(|l| *l != id);
                                                        } else {
                                                            t.push(id);
                                                        }
                                                    });
                                                },
                                                if on { icons::Check {} }
                                            }
                                            span { class: "flex flex-col gap-0.5",
                                                b { "{library.name}" }
                                                span { class: "remember-line", {items_line(library.kind, library.items)} }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button {
                        class: "btn-dialog btn-dialog-grey",
                        r#type: "button",
                        onclick: {
                            let peer = peer.clone();
                            move |_| on_done.call(peer.clone())
                        },
                        "Skip"
                    }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "button", disabled: busy() || ticked().is_empty(), onclick: move |_| save.call(()), "Save" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| save.call(()) }
    }
}

/// What a paired server's own page shows: its detail, what this server
/// shares with it and could, and this server's name.
#[derive(Debug, Clone, PartialEq)]
struct PeerPage {
    detail: PeerDetail,
    grants: Vec<GrantEntry>,
    libraries: Vec<LibraryEntry>,
    me: String,
}

async fn peer_page(id: String) -> Result<PeerPage, ApiError> {
    let client = rpc::client().await?;
    let detail = client.peer_detail(id.clone()).await.map_err(rpc::error)?;
    // An unpaired server shares nothing either way.
    let (grants, libraries) = if detail.peer.revoked_at.is_none() {
        (
            client.peer_grant_list(id).await.map_err(rpc::error)?,
            client.libraries().await.map_err(rpc::error)?.libraries,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let me = client.server_status().await.map_err(rpc::error)?.name;
    Ok(PeerPage {
        detail,
        grants,
        libraries,
        me,
    })
}

/// A paired server's own page (the WebFederationPeer board), linked from
/// its row on the Federation page.
#[component]
pub fn FederationPeer(id: String) -> Element {
    let mut page = use_resource(use_reactive((&id,), |(id,)| peer_page(id)));
    let toast = use_signal(|| None::<Said>);
    let current = page.read().clone();
    rsx! {
        div { class: "flex flex-col gap-6",
            Link { class: "back-link", to: Route::Federation {}, icons::ChevronLeft {} "Federation" }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(error)) => rsx! { Alert { text: error.explain() } },
                Some(Ok(shown)) => rsx! {
                    PeerView { key: "{shown.detail.peer.id}", page: shown, toast, on_changed: move |_| page.restart() }
                },
            }
        }
        Toast { said: toast }
    }
}

#[component]
fn PeerView(page: PeerPage, toast: Signal<Option<Said>>, on_changed: EventHandler<()>) -> Element {
    let now = now();
    let PeerPage {
        detail,
        grants,
        libraries,
        me,
    } = page;
    let peer = detail.peer.clone();
    let live = peer.revoked_at.is_none();
    let online = peer.connected;
    let mut asked = use_signal(|| None::<Share>);
    // Turning a switch off asks first, as Revoke does: by id and name.
    let mut revoking = use_signal(|| None::<(String, String)>);
    let mut unpairing = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let confirming_share = use_signal(|| None::<String>);
    let confirming_unpair = use_signal(|| None::<String>);
    let go_share = {
        let peer = peer.clone();
        use_callback(move |()| {
            let Some(share) = asked() else {
                return;
            };
            let peer = peer.clone();
            busy.set(true);
            spawn(async move {
                match apply_share(&peer.id, &share).await {
                    Ok(()) => {
                        let (library, now_shared) = match &share {
                            Share::Add(l) => (l, true),
                            Share::Revoke(l) => (l, false),
                        };
                        say(toast, true, shared(&peer, library, now_shared));
                    }
                    Err(e) if asks_to_confirm(&e, confirming_share, CONFIRM_SHARE) => {
                        busy.set(false);
                        return;
                    }
                    Err(e) => say(toast, false, e.explain()),
                }
                busy.set(false);
                asked.set(None);
                revoking.set(None);
                on_changed.call(());
            });
        })
    };
    let go_unpair = {
        let peer = peer.clone();
        use_callback(move |()| {
            let peer = peer.clone();
            busy.set(true);
            spawn(async move {
                match change(&Change::Unpair(peer)).await {
                    Ok(text) => say(toast, true, text),
                    Err(e) if asks_to_confirm(&e, confirming_unpair, CONFIRM_ABOUT) => {
                        busy.set(false);
                        return;
                    }
                    Err(e) => say(toast, false, e.explain()),
                }
                busy.set(false);
                unpairing.set(false);
                on_changed.call(());
            });
        })
    };
    // The fingerprint typed under Expected new key, and the change asked
    // for: `Some(None)` clears it (M8-06).
    let mut expect_typed = use_signal(String::new);
    let mut expect_error = use_signal(|| None::<String>);
    let mut expect_asked = use_signal(|| None::<Option<String>>);
    let confirming_expect = use_signal(|| None::<String>);
    let go_expect = {
        let peer = peer.clone();
        use_callback(move |()| {
            let Some(fingerprint) = expect_asked() else {
                return;
            };
            let peer = peer.clone();
            busy.set(true);
            expect_error.set(None);
            spawn(async move {
                let set = fingerprint.is_some();
                let done = match rpc::client().await {
                    Ok(c) => c
                        .key_expect(peer.id.clone(), fingerprint)
                        .await
                        .map_err(rpc::error),
                    Err(e) => Err(e),
                };
                match done {
                    Ok(_) => {
                        say(
                            toast,
                            true,
                            if set {
                                format!("{}'s new key is expected", peer.name)
                            } else {
                                format!("No new key is expected from {}", peer.name)
                            },
                        );
                        expect_typed.set(String::new());
                    }
                    Err(e) if asks_to_confirm(&e, confirming_expect, CONFIRM_EXPECT) => {
                        busy.set(false);
                        return;
                    }
                    Err(e) => expect_error.set(Some(e.explain())),
                }
                busy.set(false);
                expect_asked.set(None);
                on_changed.call(());
            });
        })
    };
    let name_of = |id: &str| {
        libraries
            .iter()
            .find(|l| l.id == id)
            .map_or_else(|| id.to_owned(), |l| l.name.clone())
    };
    let shared_titles: Vec<String> = grants.iter().map(|g| name_of(&g.library)).collect();
    let summed = week(&detail.week);
    rsx! {
        div { class: "flex items-center gap-3.5",
            span { class: "tie-icon", icons::Server { size: 24 } }
            div { class: "flex min-w-0 flex-col gap-1",
                h1 { class: "page-title page-title-admin", "{peer.name}" }
                span { class: "flex flex-wrap items-center gap-x-2 gap-y-1 text-sm text-ctp-subtext0",
                    span { {paired_line(now, &peer)} }
                    "·"
                    span { class: "mono", title: "Key {peer.endpoint_id}", "{peer.fingerprint}" }
                    if live {
                        "·"
                        span { class: "live",
                            span { class: if online { "live-dot" } else { "live-dot live-dot-off" } }
                            {presence(now, &detail)}
                        }
                    }
                }
            }
        }
        for state in states(now, &me, &detail) {
            div { class: if state.bad { "alert alert-error" } else { "alert alert-warning" }, role: "status",
                icons::Warning {}
                span { class: "flex flex-col gap-0.5",
                    b { "{state.title}" }
                    span { "{state.text}" }
                }
            }
        }
        for key in peer.held_keys.iter().filter(|k| live && k.rejected_at.is_none()) {
            div { class: "alert alert-warning", role: "status",
                icons::Warning {}
                span { class: "flex flex-col gap-0.5",
                    b { "{peer.name} presents a new key: {key.fingerprint}" }
                    span {
                        {new_key_line(now, key)}
                        " "
                        Link { to: Route::Federation {}, "Confirm or reject it on the Federation page." }
                    }
                }
            }
        }
        if live {
            section { class: "flex flex-col gap-2.5",
                div { class: "section-head",
                    h2 { "Expected new key" }
                    span { "When {peer.name} moves to a new key, ask its admin for the new fingerprint and enter it here: that key is let in to wait for you even if others fill the places." }
                }
                if let Some(expected) = peer.expected_key.clone() {
                    div { class: "card tie-list",
                        div { class: "tie-row",
                            span { class: "tie-what",
                                b { class: "mono", "{expected.fingerprint}" }
                                span { class: "tie-sub", {expected_line(now, &peer.name, &expected)} }
                            }
                            button {
                                class: "btn",
                                r#type: "button",
                                disabled: busy(),
                                onclick: move |_| {
                                    expect_asked.set(Some(None));
                                    go_expect.call(());
                                },
                                "Clear"
                            }
                        }
                    }
                } else {
                    form {
                        class: "flex flex-wrap items-end gap-2.5",
                        onsubmit: move |e| {
                            e.prevent_default();
                            let typed = expect_typed().trim().to_owned();
                            if typed.is_empty() {
                                expect_error.set(Some("Type the fingerprint its admin reads you.".into()));
                                return;
                            }
                            expect_asked.set(Some(Some(typed)));
                            go_expect.call(());
                        },
                        Field { label: "New key's fingerprint", value: expect_typed, placeholder: "K7QX-3MPA-9TRD-WF2H", error: expect_error() }
                        button { class: "btn", r#type: "submit", disabled: busy(), "Expect" }
                    }
                }
            }
            section { class: "flex flex-col gap-2.5",
                div { class: "section-head",
                    h2 { "What you share with {peer.name}" }
                    span { "One switch per library: shared means they can browse it and watch it. {peer.name} decides which of its users get it." }
                }
                div { class: "table-card",
                    table { class: "table",
                        thead {
                            tr {
                                th { "Library" }
                                th { "Items" }
                                th { class: "text-right", "Shared" }
                            }
                        }
                        tbody {
                            for library in libraries.clone() {
                                {
                                    let on = grants.iter().any(|g| g.library == library.id);
                                    let named = (library.id.clone(), library.name.clone());
                                    rsx! {
                                        tr { key: "{library.id}",
                                            td { b { "{library.name}" } }
                                            td { class: "quiet", {items_line(library.kind, library.items)} }
                                            td { class: "end",
                                                button {
                                                    class: "switch",
                                                    r#type: "button",
                                                    role: "switch",
                                                    aria_checked: "{on}",
                                                    aria_label: "Share {library.name} with {peer.name}",
                                                    disabled: busy(),
                                                    onclick: move |_| {
                                                        if on {
                                                            revoking.set(Some(named.clone()));
                                                        } else {
                                                            asked.set(Some(Share::Add(named.0.clone())));
                                                            go_share.call(());
                                                        }
                                                    },
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
            section { class: "flex flex-col gap-2.5",
                div { class: "section-head",
                    h2 { "What {peer.name} shares with you" }
                    span { "Kept here so your users can browse it even while {peer.name} is offline." }
                }
                div { class: "card tie-list",
                    if detail.received.is_empty() {
                        div { class: "tie-row", span { class: "tie-sub", "{peer.name} shares nothing with this server yet." } }
                    }
                    for (i, library) in detail.received.iter().enumerate() {
                        {
                            let (fresh, good) = freshness(now, library, online);
                            rsx! {
                                div { key: "{library.id}", class: "tie-row",
                                    span { class: "tie-what",
                                        b { "{library.title}" }
                                        span { class: "tie-sub", {received_line(library, (i == 0).then_some(detail.cached_bytes))} }
                                    }
                                    span { class: "live",
                                        span { class: if good { "live-dot" } else { "live-dot live-dot-off" } }
                                        "{fresh}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        section { class: "flex flex-col gap-2.5",
            div { class: "section-head",
                h2 { "Plays" }
                span { "This week" }
            }
            div { class: "pb-stats",
                div { class: "pb-stat",
                    span { class: "text-xs text-ctp-subtext0", "From {peer.name}" }
                    b { "{summed.here}" }
                    span { class: "text-xs text-ctp-subtext0", {from_line(&peer, summed)} }
                }
                div { class: "pb-stat",
                    span { class: "text-xs text-ctp-subtext0", "Your users on {peer.name}" }
                    b { "{summed.there}" }
                    span { class: "text-xs text-ctp-subtext0", "progress kept on {me}" }
                }
            }
        }
        if live {
            section { class: "flex flex-col gap-2.5",
                div { class: "section-head",
                    h2 { "Unpair" }
                    span { {unpair_means(&peer, &me)} }
                }
                div {
                    button { class: "btn btn-sever", onclick: move |_| unpairing.set(true),
                        icons::Unlink { size: 14 }
                        "Unpair {peer.name}"
                    }
                }
            }
        }
        if let Some((library, named)) = revoking() {
            DangerDialog {
                title: revoke_question(&peer, &named).0,
                text: revoke_question(&peer, &named).1,
                icon: rsx! { icons::Server { size: 22 } },
                cancel: "Cancel",
                confirm: "Stop sharing",
                busy: busy(),
                on_cancel: move |()| revoking.set(None),
                on_confirm: move |()| {
                    asked.set(Some(Share::Revoke(library.clone())));
                    go_share.call(());
                },
            }
        }
        if unpairing() {
            DangerDialog {
                title: unpair_question(&peer, Some(&shared_titles)).0,
                text: unpair_question(&peer, Some(&shared_titles)).1,
                icon: rsx! { icons::Server { size: 22 } },
                cancel: "Cancel",
                confirm: "Unpair",
                busy: busy(),
                on_cancel: move |()| unpairing.set(false),
                on_confirm: move |()| go_unpair.call(()),
            }
        }
        ConfirmItsYou { asking: confirming_share, on_confirmed: move |_| go_share.call(()) }
        ConfirmItsYou { asking: confirming_unpair, on_confirmed: move |_| go_unpair.call(()) }
        ConfirmItsYou { asking: confirming_expect, on_confirmed: move |_| go_expect.call(()) }
    }
}
