//! Users and devices › Federation (P8-01, experimental): the servers
//! paired with this one, each with its key's fingerprint to compare with
//! what that server's admin sees, and Unpair; a paired server's new key,
//! held until it is confirmed or rejected; inviting a server (a
//! `federation` invite, shown as Invites shows one) and pairing with
//! another by its invite link. Each live pair says whether it is online,
//! when what it shares was last synced, with Refresh (P8-02), and its
//! plays, one number each way and no user named; and lists the libraries
//! shared with it, with Share and Revoke (P8-03). Every change asks
//! "Confirm it's you" when the sign-in is old.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, FederationApiClient, GrantEntry, InviteCreated, InviteKind, LibraryEntry,
    NewInvite, PeerEntry, PeerKey,
};

use super::admin::{Said, Toast, say};
use super::invites::InviteResult;
use super::optimized::DangerDialog;
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{Alert, Field, icons, now, rpc};
use crate::federation::{
    CONFIRM_ABOUT, CONFIRM_SHARE, key_change_error, new_key_line, peer_line, refusal,
    revoke_question, shared, state_line, unpair_question, unpaired,
};
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
                b { "{peer.name}" }
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
                let done = async {
                    let client = rpc::client().await?;
                    match &share {
                        Share::Add(library) => client
                            .peer_grant_add(peer.id.clone(), library.clone())
                            .await
                            .map(|_| ())
                            .map_err(rpc::error),
                        Share::Revoke(library) => client
                            .peer_grant_revoke(peer.id.clone(), library.clone())
                            .await
                            .map_err(rpc::error),
                    }
                }
                .await;
                match done {
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
            let (title, text) = unpair_question(peer);
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

/// Paste another server's invite link, and pair.
#[component]
fn PairDialog(on_done: EventHandler<Option<PeerEntry>>) -> Element {
    let link = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let attempt = use_callback(move |()| {
        let typed = link().trim().to_owned();
        if typed.is_empty() {
            error.set(Some("Paste the other server's invite link.".into()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn(async move {
            let paired = async {
                let client = rpc::client().await?;
                client.redeem(typed).await.map_err(rpc::error)
            }
            .await;
            match paired {
                Ok(peer) => on_done.call(Some(peer)),
                Err(e) if asks_to_confirm(&e, confirming, CONFIRM_ABOUT) => {}
                Err(e) => error.set(Some(refusal(&e))),
            }
            busy.set(false);
        });
    });
    rsx! {
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
                p { "Paste the invite link its admin made on their Federation page. This server dials it, and both keep the pair." }
                Field { label: "Invite link", value: link, placeholder: "https://spoolframe.app/i#server=…" }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(), "Pair" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| attempt.call(()) }
    }
}
