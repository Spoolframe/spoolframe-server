//! Users and devices › Invites (P6-01), as the design's WebInvites,
//! WebInviteReauth, WebInviteCreate, WebInviteResult and WebInvitesPhone
//! boards lay it out: the invites that can still make an account, making
//! one (its code shown once, with its QR code and link), revoking one, and
//! the server's own link without a code. Below them, the guests: portable
//! users granted access here (P7-06), who are no users of this server.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, InviteCreated, InviteEntry, InviteKind, PortableGrant, Role, ServerLink,
};
use wasm_bindgen::{JsCast, JsValue};

use super::admin::{Said, Toast, say};
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{Alert, Field, Notice, icons, now, role_name, rpc};
use crate::invites::{
    EXPIRY_CHOICES, InviteForm, card_line, day_month, day_month_time, expires_in, expires_soon,
    name, new_invite, qr, revoke_error, revoke_question, title, uses_chip, uses_left,
};
use crate::logic::ApiError;

/// What making an invite is, for "Confirm it's you".
const CONFIRMING: &str = "making an invite: its code lets someone create an account here";

async fn invites() -> Result<Vec<InviteEntry>, ApiError> {
    let client = rpc::client().await?;
    client.invite_list().await.map_err(rpc::error)
}

async fn grants() -> Result<Vec<PortableGrant>, ApiError> {
    let client = rpc::client().await?;
    client.grant_list().await.map_err(rpc::error)
}

async fn server_link() -> Result<ServerLink, ApiError> {
    let client = rpc::client().await?;
    client.invite_link().await.map_err(rpc::error)
}

/// Unix seconds `at` moved by the browser's offset from UTC there, for
/// the day and time the page shows.
fn local(at: i64) -> i64 {
    let date = js_sys::Date::new(&JsValue::from_f64(at as f64 * 1000.0));
    at - (date.get_timezone_offset() * 60.0) as i64
}

fn copy(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

/// The browser's share sheet, where it has one (a phone): `navigator.share`
/// is not in every browser, so it is looked up, not assumed.
fn share_sheet() -> Option<js_sys::Function> {
    let navigator = web_sys::window()?.navigator();
    let share = js_sys::Reflect::get(&navigator, &JsValue::from_str("share")).ok()?;
    share.dyn_into::<js_sys::Function>().ok()
}

fn share(link: &str) {
    let (Some(window), Some(share)) = (web_sys::window(), share_sheet()) else {
        return;
    };
    let data = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&data, &JsValue::from_str("url"), &JsValue::from_str(link));
    let _ = share.call1(&window.navigator(), &data);
}

#[component]
pub fn Invites() -> Element {
    let mut list = use_resource(invites);
    let toast = use_signal(|| None::<Said>);
    let mut creating = use_signal(|| false);
    let mut made = use_signal(|| None::<InviteCreated>);
    let mut revoking = use_signal(|| None::<InviteEntry>);
    let mut showing = use_signal(|| None::<ServerLink>);
    let now = now();
    let current = list.read().clone();
    rsx! {
        div { class: "flex flex-col gap-6",
            div { class: "page-head",
                div { class: "flex flex-col gap-1.5",
                    h1 { class: "page-title page-title-admin hidden md:block", "Invites" }
                    span { class: "hidden max-w-[680px] text-sm leading-normal text-ctp-subtext0 md:inline",
                        "An invite finds this server from anywhere and makes an account. Its code is shown once, when you make it."
                    }
                }
                button { class: "btn-add hidden md:inline-flex", onclick: move |_| creating.set(true),
                    icons::Plus { size: 16 }
                    "Create invite"
                }
            }
            button { class: "btn-primary h-[42px] md:hidden", onclick: move |_| creating.set(true),
                icons::Plus { size: 17 }
                "Create invite"
            }
            match current {
                None => rsx! { p { class: "text-ctp-subtext0", "Loading…" } },
                Some(Err(error)) => rsx! { Alert { text: error.explain() } },
                Some(Ok(invites)) if invites.is_empty() => rsx! {
                    section { class: "card empty",
                        icons::Ticket { size: 22 }
                        b { "No invites" }
                        span { "Make one to let someone find this server and make their own account." }
                    }
                },
                Some(Ok(invites)) => rsx! {
                    InviteTable {
                        invites: invites.clone(),
                        now,
                        on_revoke: move |invite: InviteEntry| revoking.set(Some(invite)),
                    }
                    div { class: "flex flex-col gap-3 md:hidden",
                        for invite in invites.iter().cloned() {
                            InviteCard {
                                key: "{invite.id}",
                                invite,
                                now,
                                on_revoke: move |invite: InviteEntry| revoking.set(Some(invite)),
                            }
                        }
                    }
                },
            }
            p { class: "m-0 -mt-2.5 mx-1 hidden text-[13px] text-ctp-subtext0 md:block",
                "Used-up and expired invites drop off this list. Revoking stops the unused codes; accounts already made stay."
            }
            ShareServer { on_show: move |link: ServerLink| showing.set(Some(link)) }
            Guests { toast }
        }
        if creating() {
            CreateInvite {
                on_done: move |created: Option<InviteCreated>| {
                    creating.set(false);
                    if let Some(created) = created {
                        made.set(Some(created));
                        list.restart();
                    }
                },
            }
        }
        if let Some(created) = made() {
            InviteResult { created, on_done: move |_| made.set(None) }
        }
        if let Some(invite) = revoking() {
            RevokeInvite {
                invite,
                on_done: move |said: Option<(bool, String)>| {
                    revoking.set(None);
                    if let Some((ok, text)) = said {
                        say(toast, ok, text);
                        list.restart();
                    }
                },
            }
        }
        if let Some(link) = showing() {
            LinkDialog { link: link.link, share_link: link.share_link, on_done: move |_| showing.set(None) }
        }
        Toast { said: toast }
    }
}

/// The role as the list's pill shows it; "Guest" for a portable guest's
/// invite (P7-05), which makes no account.
#[component]
fn RolePill(role: Role, #[props(default)] guest: bool, #[props(default)] server: bool) -> Element {
    // A server's pairing invite (P8-01) makes no account either.
    if server {
        return rsx! {
            span { class: "pill", "Server" }
        };
    }
    if guest {
        return rsx! {
            span { class: "pill", "Guest" }
        };
    }
    rsx! {
        span { class: if role == Role::Admin { "pill pill-admin" } else { "pill" }, "{role_name(role)}" }
    }
}

/// WebInvites' table: who it is for, the role, the uses left, when it
/// expires, who made it, and Revoke.
#[component]
fn InviteTable(
    invites: Vec<InviteEntry>,
    now: i64,
    on_revoke: EventHandler<InviteEntry>,
) -> Element {
    rsx! {
        section { class: "table-card hidden md:block",
            table { class: "table",
                thead {
                    tr {
                        th { "Note" }
                        th { "Role" }
                        th { "Uses left" }
                        th { "Expires" }
                        th { "Made by" }
                        th {}
                    }
                }
                tbody {
                    for invite in invites {
                        tr { key: "{invite.id}",
                            td {
                                div { class: "invite-name",
                                    span { class: "invite-icon", icons::Ticket {} }
                                    b { "{name(&invite)}" }
                                }
                            }
                            td {
                                RolePill {
                                    role: invite.role,
                                    guest: invite.kind == InviteKind::PortableAccount,
                                    server: invite.kind == InviteKind::Federation,
                                }
                            }
                            td { "{uses_left(&invite)}" }
                            td {
                                div { class: "invite-when",
                                    span {
                                        "{expires_in(now, invite.expires_at)}"
                                        if expires_soon(now, invite.expires_at) {
                                            span { class: "invite-soon", " · soon" }
                                        }
                                    }
                                    span { "{day_month_time(local(invite.expires_at))}" }
                                }
                            }
                            td { class: if invite.created_by == spoolframe_admin_api::invite::MADE_BY_CLI { "quiet" } else { "" },
                                "{invite.created_by}"
                            }
                            td { class: "end",
                                button {
                                    class: "btn-revoke",
                                    onclick: {
                                        let invite = invite.clone();
                                        move |_| on_revoke.call(invite.clone())
                                    },
                                    "Revoke"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// WebInvitesPhone 1: an invite as a card.
#[component]
fn InviteCard(invite: InviteEntry, now: i64, on_revoke: EventHandler<InviteEntry>) -> Element {
    rsx! {
        div { class: "invite-card",
            div { class: "flex items-center gap-2",
                b { class: "flex-grow", "{name(&invite)}" }
                RolePill { role: invite.role, guest: invite.kind == InviteKind::PortableAccount, server: invite.kind == InviteKind::Federation }
            }
            span { "{card_line(&invite, now)}" }
            button {
                class: "btn-revoke",
                onclick: {
                    let invite = invite.clone();
                    move |_| on_revoke.call(invite.clone())
                },
                "Revoke"
            }
        }
    }
}

/// "Share the server without an account": the server's link, which has
/// no code, never expires and needs no confirming. Copy copies its https
/// form; Show QR shows the `spoolframe://` one.
#[component]
fn ShareServer(on_show: EventHandler<ServerLink>) -> Element {
    let link = use_resource(server_link);
    let mut copied = use_signal(|| false);
    let current = link.read().clone();
    rsx! {
        section { class: "invite-share",
            span { class: "invite-share-icon hidden md:flex", icons::LinkChain { size: 22 } }
            div { class: "flex min-w-0 flex-[1_1_360px] flex-col gap-1",
                b { class: "text-[15px]", "Share the server without an account" }
                span { class: "hidden text-[13px] leading-normal text-ctp-subtext0 md:inline",
                    "This link has no code. It lets an app find this server and save it; the person still signs in with an account they already have. It never expires and needs no confirming."
                }
                span { class: "text-[13px] leading-normal text-ctp-subtext0 md:hidden",
                    "No code: it finds and saves the server only."
                }
            }
            match current {
                None => rsx! { span { class: "text-sm text-ctp-subtext0", "Loading…" } },
                Some(Err(error)) => rsx! { span { class: "text-sm text-ctp-red", "{error.explain()}" } },
                Some(Ok(link)) => rsx! {
                    div { class: "flex gap-2",
                        button {
                            class: "btn",
                            onclick: {
                                let link = link.clone();
                                move |_| on_show.call(link.clone())
                            },
                            icons::QrCode {}
                            "Show QR"
                        }
                        button {
                            class: "btn",
                            onclick: {
                                let link = link.share_link.clone();
                                move |_| {
                                    copy(&link);
                                    copied.set(true);
                                }
                            },
                            icons::Copy {}
                            if copied() { "Copied" } else { "Copy link" }
                        }
                    }
                },
            }
        }
    }
}

/// The guests: portable users granted access here (P7-06). They sign in
/// with their own identity, from their phone, and are not users of this
/// server: they never show in Users and devices. Revoking ends their
/// sessions at once.
#[component]
fn Guests(toast: Signal<Option<Said>>) -> Element {
    let mut list = use_resource(grants);
    let mut revoking = use_signal(|| None::<PortableGrant>);
    let current = list.read().clone();
    let Some(Ok(guests)) = current else {
        return rsx! {};
    };
    if guests.is_empty() {
        return rsx! {};
    }
    rsx! {
        section { class: "table-card",
            div { class: "flex flex-col gap-1 p-4",
                b { class: "text-[15px]", "Guests with their own identity" }
                span { class: "text-[13px] leading-normal text-ctp-subtext0",
                    "They watch here as themselves, signed in from their own phone. This server keeps only their public id."
                }
            }
            table { class: "table",
                thead {
                    tr {
                        th { "Name" }
                        th { "Fingerprint" }
                        th { "Granted" }
                        th { "Signed in" }
                        th {}
                    }
                }
                tbody {
                    for guest in guests {
                        tr { key: "{guest.id}",
                            td {
                                div { class: "invite-name",
                                    span { class: "invite-icon", icons::User {} }
                                    b { "{guest_name(&guest)}" }
                                }
                            }
                            td { class: "quiet", "{guest.fingerprint}" }
                            td { "{day_month(local(guest.granted_at))}" }
                            td { "{guest.live_sessions}" }
                            td { class: "end",
                                button {
                                    class: "btn-revoke",
                                    onclick: {
                                        let guest = guest.clone();
                                        move |_| revoking.set(Some(guest.clone()))
                                    },
                                    "Revoke"
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(guest) = revoking() {
            RevokeGrant {
                guest,
                on_done: move |said: Option<(bool, String)>| {
                    revoking.set(None);
                    if let Some((ok, text)) = said {
                        say(toast, ok, text);
                        list.restart();
                    }
                },
            }
        }
    }
}

/// What a guest is called: the name they asked for, else their
/// fingerprint.
fn guest_name(guest: &PortableGrant) -> String {
    guest
        .display_name
        .clone()
        .unwrap_or_else(|| guest.fingerprint.clone())
}

/// "Revoke Sam's access?". From a session signed in long ago, "Confirm
/// it's you" first.
#[component]
fn RevokeGrant(guest: PortableGrant, on_done: EventHandler<Option<(bool, String)>>) -> Element {
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let shown = guest_name(&guest);
    let id = guest.id.clone();
    let attempt = use_callback({
        let shown = shown.clone();
        move |()| {
            busy.set(true);
            let id = id.clone();
            let shown = shown.clone();
            spawn(async move {
                let revoked = async {
                    let client = rpc::client().await?;
                    client.grant_revoke(id).await.map_err(rpc::error)
                };
                match revoked.await {
                    Ok(()) => on_done.call(Some((true, format!("{shown}: access revoked")))),
                    Err(e) if asks_to_confirm(&e, confirming, "revoking a guest's access") => {}
                    Err(e) => error.set(Some(e.explain())),
                }
                busy.set(false);
            });
        }
    });
    rsx! {
        div { class: "scrim scrim-sheet",
            section { class: "dialog", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::User { size: 22 }
                    h1 { "Revoke {shown}'s access?" }
                }
                p { "They are signed out everywhere at once, and only a new invite lets them back. What they watched stays, for if one does." }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Keep it" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: move |_| attempt.call(()), "Revoke access" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| attempt.call(()) }
    }
}

/// A QR code of `data`, `px` pixels a side, with the mark in the middle,
/// as the design draws it: dark on light, whatever the theme.
#[component]
fn QrCard(data: String, px: u32) -> Element {
    let Some(code) = qr(&data) else {
        return rsx! {
            p { class: "text-sm text-ctp-subtext0", "This link is too long for a QR code: send the link." }
        };
    };
    rsx! {
        div { class: "invite-qr",
            div { class: "invite-qr-inner",
                svg {
                    width: "{px}",
                    height: "{px}",
                    view_box: "0 0 {code.size} {code.size}",
                    shape_rendering: "crispEdges",
                    role: "img",
                    "aria-label": "QR code of the invite link",
                    path { fill: "#11111b", d: "{code.path}" }
                }
                div { class: "invite-qr-mark", icons::Mark { size: 32 } }
            }
        }
    }
}

/// WebInviteCreate: who it is for, how many accounts, how long, which
/// role. From a session signed in long ago the server asks to confirm it
/// is the admin first (WebInviteReauth), and the invite is then made.
#[component]
fn CreateInvite(on_done: EventHandler<Option<InviteCreated>>) -> Element {
    let note = use_signal(String::new);
    let uses = use_signal(|| "1".to_owned());
    let mut expires = use_signal(|| InviteForm::default().expires);
    let mut admin = use_signal(|| false);
    let mut guest = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let attempt = use_callback(move |()| {
        let form = InviteForm {
            note: note(),
            uses: uses(),
            expires: expires(),
            role: if admin() { Role::Admin } else { Role::Consumer },
            kind: if guest() {
                InviteKind::PortableAccount
            } else {
                InviteKind::CreateUser
            },
        };
        let invite = match new_invite(&form) {
            Ok(invite) => invite,
            Err(text) => {
                error.set(Some(text));
                return;
            }
        };
        busy.set(true);
        spawn(async move {
            let made = async {
                let client = rpc::client().await?;
                client.invite_create(invite).await.map_err(rpc::error)
            };
            match made.await {
                Ok(created) => on_done.call(Some(created)),
                Err(e) if asks_to_confirm(&e, confirming, CONFIRMING) => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let submit = move |e: FormEvent| {
        e.prevent_default();
        attempt.call(());
    };
    rsx! {
        div { class: "scrim scrim-sheet",
            form { class: "dialog dialog-wide", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    icons::Ticket { size: 22 }
                    h1 { "Create invite" }
                }
                Field { label: "Note", value: note, autocomplete: "off", placeholder: "Who it is for" }
                span { class: "hint", "Only admins see it. It says who the invite is for." }
                div { class: "grid grid-cols-2 gap-3",
                    label { class: "field",
                        "Uses"
                        input {
                            r#type: "number",
                            min: "1",
                            max: "{spoolframe_admin_api::invite::MAX_USES}",
                            value: "{uses}",
                            oninput: {
                                let mut uses = uses;
                                move |e: FormEvent| uses.set(e.value())
                            },
                        }
                        span { class: "text-xs font-normal text-ctp-subtext0", "Accounts it can make" }
                    }
                    label { class: "field",
                        "Expires after"
                        select {
                            onchange: move |e| {
                                if let Ok(secs) = e.value().parse() {
                                    expires.set(secs);
                                }
                            },
                            for (secs, label) in EXPIRY_CHOICES {
                                option { value: "{secs}", selected: secs == expires(), "{label}" }
                            }
                        }
                        span { class: "text-xs font-normal text-ctp-subtext0", "1 hour to 30 days" }
                    }
                }
                label { class: "field",
                    "For"
                    select { onchange: move |e| guest.set(e.value() == "guest"),
                        option { value: "account", selected: !guest(), "An account on this server" }
                        option { value: "guest", selected: guest(), "A guest with their own identity" }
                    }
                }
                if guest() {
                    span { class: "hint",
                        "They join from their phone with their own Spoolframe identity, and watch here as themselves. No account is made; they show under Guests."
                    }
                } else {
                    label { class: "field",
                        "Role"
                        select { onchange: move |e| admin.set(e.value() == "admin"),
                            option { value: "consumer", selected: !admin(), "Consumer" }
                            option { value: "admin", selected: admin(), "Admin" }
                        }
                    }
                    span { class: "hint", "Consumers watch and listen. Admins can change the server." }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(), "Create invite" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| attempt.call(()) }
    }
}

/// WebInviteResult: the code, shown this once, its QR code and its link,
/// and what the invite does. On a phone (WebInvitesPhone 4) the link goes
/// out through the share sheet. The QR code is the `spoolframe://` link,
/// scanned in the same room; what is shown, copied and shared is the
/// https one (PX-45), which a message makes tappable.
#[component]
pub(super) fn InviteResult(created: InviteCreated, on_done: EventHandler<()>) -> Element {
    let mut copied = use_signal(|| false);
    let invite = &created.invite;
    let link = created.share_link.clone();
    let can_share = share_sheet().is_some();
    let copy_link = {
        let link = link.clone();
        move |_| {
            copy(&link);
            copied.set(true);
        }
    };
    rsx! {
        div { class: "scrim scrim-sheet",
            section { class: "dialog dialog-result", role: "dialog",
                div { class: "dialog-head",
                    span { class: "text-ctp-green", icons::Check { size: 22 } }
                    h1 { "{title(invite.note.as_deref())}" }
                }
                div { class: "flex flex-col items-center gap-6 md:flex-row md:items-start",
                    div { class: "flex flex-col items-center gap-2.5",
                        QrCard { data: created.link.clone(), px: 204 }
                        span { class: "hidden text-xs text-ctp-subtext0 md:inline", "Scan with the Spoolframe app" }
                    }
                    div { class: "flex w-full min-w-0 flex-1 flex-col gap-3.5",
                        div { class: "flex flex-col gap-1",
                            span { class: "invite-label hidden md:inline", "Code" }
                            span { class: "invite-code", "{created.code}" }
                        }
                        div { class: "hidden flex-col gap-1.5 md:flex",
                            span { class: "invite-label", "Link: send it, or paste it into the app’s Add a server" }
                            div { class: "flex items-stretch gap-2",
                                div { class: "invite-link min-w-0 flex-1", "{link}" }
                                button { class: "btn shrink-0", onclick: copy_link.clone(),
                                    icons::Copy {}
                                    if copied() { "Copied" } else { "Copy" }
                                }
                            }
                        }
                        div { class: "grid grid-cols-2 gap-2 md:hidden",
                            if can_share {
                                button {
                                    class: "btn w-full justify-center",
                                    onclick: {
                                        let link = link.clone();
                                        move |_| share(&link)
                                    },
                                    icons::Share {}
                                    "Share link"
                                }
                            }
                            button { class: "btn w-full justify-center", onclick: copy_link.clone(),
                                icons::Copy {}
                                if copied() { "Copied" } else { "Copy link" }
                            }
                        }
                        div { class: "flex flex-wrap gap-2",
                            if invite.kind == InviteKind::PortableAccount {
                                span { class: "pill", "Guest with their own identity" }
                            } else if invite.kind == InviteKind::Federation {
                                span { class: "pill", "Pairs a server" }
                            } else {
                                span { class: "pill", "{role_name(invite.role)}" }
                            }
                            span { class: "pill", "{uses_chip(invite.uses)}" }
                            span { class: "pill hidden md:inline", "Expires {day_month_time(local(invite.expires_at))}" }
                            span { class: "pill md:hidden", "Expires {day_month(local(invite.expires_at))}" }
                        }
                    }
                }
                Notice {
                    span { class: "hidden text-ctp-text md:inline",
                        "This is the only time the code is shown: the server keeps a hash of it. Copy or send it now. Lost it? Revoke this invite and make another."
                    }
                    span { class: "text-ctp-text md:hidden", "Shown once. Send it now, or revoke and make another." }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-outline hidden md:inline-flex", onclick: copy_link,
                        icons::Copy {}
                        "Copy link"
                    }
                    button { class: "btn-dialog btn-dialog-primary w-full md:w-auto", onclick: move |_| on_done.call(()), "Done" }
                }
            }
        }
    }
}

/// The server's link without a code, as a QR code ("Show QR"): `link`,
/// the `spoolframe://` form; the https `share_link` is shown and copied.
#[component]
fn LinkDialog(link: String, share_link: String, on_done: EventHandler<()>) -> Element {
    let mut copied = use_signal(|| false);
    rsx! {
        div { class: "scrim scrim-sheet",
            section { class: "dialog dialog-wide", role: "dialog",
                div { class: "dialog-head",
                    icons::LinkChain { size: 22 }
                    h1 { "This server's link" }
                }
                div { class: "flex justify-center",
                    QrCard { data: link.clone(), px: 204 }
                }
                p { "Scan it with the Spoolframe app, or send the link. It finds and saves this server; the person signs in with an account they already have." }
                div { class: "invite-link", "{share_link}" }
                div { class: "dialog-actions",
                    button {
                        class: "btn-dialog btn-dialog-outline",
                        onclick: {
                            let link = share_link.clone();
                            move |_| {
                                copy(&link);
                                copied.set(true);
                            }
                        },
                        icons::Copy {}
                        if copied() { "Copied" } else { "Copy link" }
                    }
                    button { class: "btn-dialog btn-dialog-primary", onclick: move |_| on_done.call(()), "Done" }
                }
            }
        }
    }
}

/// WebInvitesPhone 5: "Revoke the Book club invite?". From a session
/// signed in long ago, "Confirm it's you" first.
#[component]
fn RevokeInvite(invite: InviteEntry, on_done: EventHandler<Option<(bool, String)>>) -> Element {
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let (question, body) = revoke_question(&invite);
    let id = invite.id.clone();
    let shown = name(&invite);
    let attempt = use_callback(move |()| {
        busy.set(true);
        let id = id.clone();
        let shown = shown.clone();
        spawn(async move {
            let revoked = async {
                let client = rpc::client().await?;
                client.invite_revoke(id).await.map_err(rpc::error)
            };
            match revoked.await {
                Ok(()) => on_done.call(Some((true, format!("{shown}: invite revoked")))),
                Err(e) if asks_to_confirm(&e, confirming, "revoking an invite") => {}
                Err(e) => error.set(Some(revoke_error(&e))),
            }
            busy.set(false);
        });
    });
    rsx! {
        div { class: "scrim scrim-sheet",
            section { class: "dialog", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::Ticket { size: 22 }
                    h1 { "{question}" }
                }
                p { "{body}" }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_done.call(None), "Keep it" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: move |_| attempt.call(()), "Revoke invite" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| attempt.call(()) }
    }
}
