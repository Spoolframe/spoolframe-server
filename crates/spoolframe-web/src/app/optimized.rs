//! Server › Optimized (PX-37): the profiles copies are made in, with Add,
//! Edit and Remove; the pool's use against its budget; the copy being
//! made; and every copy, with Delete. Editing a profile's video outdates
//! its copies, and the dialog offers "Delete them now". The admin API has
//! no subscriptions, so the page asks again every few seconds.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, OptimizedCopy, OptimizedOverview, OptimizedProfile, OptimizedProgress,
};

use super::admin::{Said, Toast, say};
use super::catalog::Forbidden;
use super::playback::use_polled;
use super::{Alert, Field, icons, now, rpc};
use crate::activity::updated_line;
use crate::logic::{ApiError, ago};
use crate::optimized::{
    CODECS, codec_label, copies_of, copy_deleted_text, copy_name, eta_text, job_percent,
    mbit_field, outdated_deleted_text, outdated_message, parse_height, parse_mbit, profile_line,
    size_text, state_text, usage,
};

async fn overview() -> Result<OptimizedOverview, ApiError> {
    let client = rpc::client().await?;
    client.optimized_overview().await.map_err(rpc::error)
}

/// A dialog open on the page.
#[derive(Debug, Clone, PartialEq)]
enum Open {
    /// Add a profile.
    Adding,
    /// Edit this profile.
    Editing(OptimizedProfile),
    /// Remove this profile, asked first.
    Removing(OptimizedProfile),
    /// Delete this copy, asked first.
    Deleting(OptimizedCopy),
}

#[component]
pub fn OptimizedPage() -> Element {
    let (seen, error, clock) = use_polled(overview);
    let mut open = use_signal(|| None::<Open>);
    let toast = use_signal(|| None::<Said>);
    let shown = seen();
    let live = updated_line(false, clock(), shown.as_ref().map(|(_, at)| *at));
    let forbidden = error()
        .as_ref()
        .is_some_and(|e| e.code == spoolframe_admin_api::error_code::FORBIDDEN);
    let on_done = move |said: Option<Result<String, String>>| {
        open.set(None);
        match said {
            Some(Ok(text)) => say(toast, true, text),
            Some(Err(text)) => say(toast, false, text),
            None => {}
        }
    };
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin hidden md:block", "Optimized" }
                span { class: "live", role: "status",
                    span { class: if error().is_some() { "live-dot live-dot-off" } else { "live-dot" } }
                    "{live}"
                }
            }
            p { class: "m-0 max-w-[820px] text-sm text-ctp-subtext0 max-md:hidden",
                "Converted copies the server keeps, so a film is converted once and played or downloaded from the copy after. Copies are made in a profile and shared by every user; binge-ahead's are marked ahead. Over the budget, outdated and ahead copies go first, then the least recently used. A copy being read stays."
            }
            if forbidden {
                Forbidden {}
            } else if let Some(e) = error() {
                Alert { text: format!("Not updating: {}", e.explain()) }
            }
            match shown {
                None if error().is_none() => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                None => rsx! {},
                Some((view, _)) => rsx! {
                    Usage { view: view.clone() }
                    Profiles { view: view.clone(), open }
                    if let Some(copy) = view.copies.iter().find(|c| c.progress.is_some()) {
                        Making { copy: copy.clone() }
                    }
                    Copies { view: view.clone(), open, toast }
                },
            }
        }
        match open() {
            Some(Open::Adding) => rsx! { ProfileDialog { editing: None, on_done } },
            Some(Open::Editing(profile)) => rsx! { ProfileDialog { editing: Some(profile), on_done } },
            Some(Open::Removing(profile)) => rsx! { RemoveDialog { profile, on_done } },
            Some(Open::Deleting(copy)) => rsx! { DeleteCopyDialog { copy, on_done } },
            None => rsx! {},
        }
        Toast { said: toast }
    }
}

/// The pool against its budget.
#[component]
fn Usage(view: OptimizedOverview) -> Element {
    let (percent, words, tone) = usage(view.used_bytes, view.max_size_bytes);
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "Space" }
                span { class: "text-[13px] text-ctp-subtext0", "{view.copies.len()} copies" }
            }
            div { class: "card-body",
                div { class: "pb-bar max-w-none h-2",
                    span { class: "pb-bar-fill {tone}", style: "width: {percent}%" }
                }
                span { class: "text-sm", b { "{words}" } " · {percent} %" }
                span { class: "text-xs text-ctp-subtext0",
                    "In "
                    span { class: "mono", "{view.dir}" }
                    ". The budget is optimized.max_size_mb in the config file."
                }
            }
        }
    }
}

#[component]
fn Profiles(view: OptimizedOverview, open: Signal<Option<Open>>) -> Element {
    let mut open = open;
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "Profiles" }
                button { class: "btn h-[30px] ml-auto", onclick: move |_| open.set(Some(Open::Adding)),
                    icons::Plus { size: 14 }
                    "Add profile"
                }
            }
            if view.profiles.is_empty() {
                div { class: "card-body",
                    p { class: "m-0 text-sm text-ctp-subtext0", "No profiles: downloads are offered in original quality only." }
                }
            } else {
                div { class: "overflow-x-auto",
                    table { class: "table",
                        thead {
                            tr {
                                th { "Name" }
                                th { "Video" }
                                th { "Copies" }
                                th {}
                            }
                        }
                        tbody {
                            for p in view.profiles.iter().cloned() {
                                tr { key: "{p.name}",
                                    td { class: "font-bold", "{p.name}" }
                                    td { "{profile_line(&p)}" }
                                    td { "{copies_of(&view.copies, &p.name)}" }
                                    td {
                                        span { class: "row-do justify-end",
                                            button { class: "btn",
                                                onclick: {
                                                    let p = p.clone();
                                                    move |_| open.set(Some(Open::Editing(p.clone())))
                                                },
                                                icons::Pencil { size: 14 }
                                                "Edit"
                                            }
                                            button { class: "btn-delete", aria_label: "Remove {p.name}",
                                                onclick: {
                                                    let p = p.clone();
                                                    move |_| open.set(Some(Open::Removing(p.clone())))
                                                },
                                                icons::Trash { size: 14 }
                                                "Remove"
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
    }
}

/// The copy being made, with how far it is: at most one is.
#[component]
fn Making(copy: OptimizedCopy) -> Element {
    let (name, _) = copy_name(&copy);
    let OptimizedProgress { fraction, eta_secs } = copy.progress.unwrap_or(OptimizedProgress {
        fraction: None,
        eta_secs: None,
    });
    let percent = job_percent(fraction);
    let words = match (percent, eta_secs) {
        (Some(p), Some(eta)) => format!("{p} % · {}", eta_text(eta)),
        (Some(p), None) => format!("{p} %"),
        (None, _) => "Starting".to_owned(),
    };
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Being made" } }
            div { class: "card-body",
                div { class: "flex flex-col gap-1",
                    span { b { "{name}" } span { class: "text-ctp-subtext0", " · {copy.target}" } }
                    div { class: "pb-bar max-w-none",
                        span { class: "pb-bar-fill act-peach", style: "width: {percent.unwrap_or(0)}%" }
                    }
                    span { class: "text-xs text-ctp-subtext0", "{words}" }
                }
            }
        }
    }
}

#[component]
fn Copies(
    view: OptimizedOverview,
    open: Signal<Option<Open>>,
    toast: Signal<Option<Said>>,
) -> Element {
    let mut open = open;
    let mut busy = use_signal(|| false);
    let outdated = view.copies.iter().filter(|c| c.outdated).count();
    let delete_outdated = move |_| {
        busy.set(true);
        spawn(async move {
            let done = async {
                let client = rpc::client().await?;
                client
                    .optimized_outdated_delete(None)
                    .await
                    .map_err(rpc::error)
            };
            match done.await {
                Ok(done) => say(toast, true, outdated_deleted_text(done)),
                Err(e) => say(toast, false, e.explain()),
            }
            busy.set(false);
        });
    };
    let now = now();
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "Copies" }
                span { class: "text-[13px] text-ctp-subtext0", "in the order they would be evicted" }
                if outdated > 0 {
                    button { class: "btn h-[30px] ml-auto", disabled: busy(), onclick: delete_outdated,
                        icons::Trash { size: 14 }
                        "Delete {outdated} outdated"
                    }
                }
            }
            if view.copies.is_empty() {
                div { class: "card-body",
                    p { class: "m-0 text-sm text-ctp-subtext0", "No copies yet. A download in a profile makes one, and so does binge-ahead." }
                }
            } else {
                div { class: "overflow-x-auto",
                    table { class: "table",
                        thead {
                            tr {
                                th { "Item" }
                                th { "Profile" }
                                th { "State" }
                                th { "Size" }
                                th { "Last used" }
                                th {}
                            }
                        }
                        tbody {
                            for c in view.copies.iter().cloned() {
                                {
                                    let (name, under) = copy_name(&c);
                                    let (state, tone) = state_text(c.state);
                                    rsx! {
                                        tr { key: "{c.id}",
                                            td {
                                                div { class: "who-name", title: "{c.path}",
                                                    b { "{name}" }
                                                    span { "{under}" }
                                                }
                                            }
                                            td {
                                                span { class: "flex flex-wrap items-center gap-1.5",
                                                    span { class: if c.target == "ahead" { "mono" } else { "" }, "{c.target}" }
                                                    if c.outdated {
                                                        span { class: "pill pill-warn", "outdated" }
                                                    }
                                                }
                                            }
                                            td {
                                                div { class: "flex flex-col",
                                                    span { class: "font-semibold {tone}", "{state}" }
                                                    if c.reading {
                                                        span { class: "text-xs text-ctp-subtext0", "being read" }
                                                    }
                                                    if let Some(why) = &c.failure {
                                                        span { class: "text-xs text-ctp-red", "{why}" }
                                                    }
                                                }
                                            }
                                            td { class: "whitespace-nowrap", "{size_text(&c)}" }
                                            td { class: "whitespace-nowrap", "{ago(now, c.last_used_at)}" }
                                            td {
                                                button { class: "btn-delete", aria_label: "Delete the copy of {name}",
                                                    onclick: {
                                                        let c = c.clone();
                                                        move |_| open.set(Some(Open::Deleting(c.clone())))
                                                    },
                                                    icons::Trash { size: 14 }
                                                    "Delete"
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
        }
    }
}

/// Add or Edit a profile. An edit that outdated copies stays open, to
/// say how many and offer "Delete them now".
#[component]
fn ProfileDialog(
    editing: Option<OptimizedProfile>,
    on_done: EventHandler<Option<Result<String, String>>>,
) -> Element {
    let start = editing.clone();
    let name = use_signal(|| start.as_ref().map_or_else(String::new, |p| p.name.clone()));
    let mut codec = use_signal(|| {
        start
            .as_ref()
            .map_or_else(|| "h264".to_owned(), |p| p.video_codec.clone())
    });
    let height = use_signal(|| {
        start
            .as_ref()
            .map_or_else(|| "1080".to_owned(), |p| p.height.to_string())
    });
    let bitrate = use_signal(|| {
        start
            .as_ref()
            .map_or_else(|| "4".to_owned(), |p| mbit_field(p.max_bitrate))
    });
    let mut failed = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // After an edit that outdated copies: the profile's name and how many.
    let mut outdated = use_signal(|| None::<(String, u64)>);
    let adding = editing.is_none();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        let fields = parse_height(&height()).and_then(|h| parse_mbit(&bitrate()).map(|b| (h, b)));
        let (height, max_bitrate) = match fields {
            Ok(fields) => fields,
            Err(why) => {
                failed.set(Some(why));
                return;
            }
        };
        let profile = OptimizedProfile {
            name: name().trim().to_owned(),
            video_codec: codec(),
            height,
            max_bitrate,
        };
        busy.set(true);
        spawn(async move {
            let written = async {
                let client = rpc::client().await?;
                client
                    .optimized_profile_put(profile.clone())
                    .await
                    .map_err(rpc::error)
            };
            match written.await {
                Ok(w) if w.outdated > 0 => outdated.set(Some((profile.name, w.outdated))),
                Ok(w) if w.created => {
                    on_done.call(Some(Ok(format!("Profile {} added", profile.name))))
                }
                Ok(_) => on_done.call(Some(Ok(format!("Profile {} saved", profile.name)))),
                Err(e) => failed.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    if let Some((profile, n)) = outdated() {
        return rsx! {
            OutdatedDialog { profile, n, on_done }
        };
    }
    rsx! {
        div { class: "scrim",
            form { class: "dialog w-[440px]", role: "dialog", onsubmit: submit,
                div { class: "dialog-head",
                    if adding { icons::Plus { size: 22 } } else { icons::Pencil { size: 22 } }
                    h1 { if adding { "Add a profile" } else { "Edit {name}" } }
                }
                if adding {
                    Field { label: "Name", value: name, autocomplete: "off", placeholder: "Tablet" }
                }
                label { class: "field",
                    "Video codec"
                    select { onchange: move |e| codec.set(e.value()),
                        for (value, label) in CODECS {
                            option { key: "{value}", value, selected: codec() == value, "{label}" }
                        }
                        if !CODECS.iter().any(|(v, _)| *v == codec()) {
                            option { value: "{codec}", selected: true, "{codec_label(&codec())}" }
                        }
                    }
                }
                Field { label: "Height, in lines", value: height, autocomplete: "off", placeholder: "1080" }
                Field { label: "Bitrate, in Mbit/s", value: bitrate, autocomplete: "off", placeholder: "4" }
                if !adding {
                    span { class: "hint", "A change to the video outdates the profile's copies: they are not used again, and go first when room is needed." }
                }
                if let Some(text) = failed() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", r#type: "button", onclick: move |_| on_done.call(None), "Cancel" }
                    button { class: "btn-dialog btn-dialog-primary", r#type: "submit", disabled: busy(),
                        if adding { "Add profile" } else { "Save" }
                    }
                }
            }
        }
    }
}

/// Run `work`, a dialog's action, with `busy` set while it runs; then
/// close the dialog saying how it went.
fn act(
    mut busy: Signal<bool>,
    on_done: EventHandler<Option<Result<String, String>>>,
    work: impl std::future::Future<Output = Result<String, ApiError>> + 'static,
) {
    busy.set(true);
    spawn(async move {
        let said = work.await.map_err(|e| e.explain());
        busy.set(false);
        on_done.call(Some(said));
    });
}

/// A dialog that asks before something that cannot be undone: `title`,
/// `text`, and two buttons. `warning` heads it with a warning rather than
/// a delete's red.
#[component]
fn DangerDialog(
    title: String,
    text: String,
    #[props(default)] warning: bool,
    cancel: String,
    confirm: String,
    busy: bool,
    on_cancel: EventHandler<()>,
    on_confirm: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: if warning { "dialog-head" } else { "dialog-head dialog-head-danger" },
                    if warning { icons::Warning { size: 22 } } else { icons::Trash { size: 22 } }
                    h1 { "{title}" }
                }
                p { "{text}" }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_cancel.call(()), "{cancel}" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy, onclick: move |_| on_confirm.call(()), "{confirm}" }
                }
            }
        }
    }
}

/// After an edit outdated `n` of `profile`'s copies: keep them for now,
/// or delete them now.
#[component]
fn OutdatedDialog(
    profile: String,
    n: u64,
    on_done: EventHandler<Option<Result<String, String>>>,
) -> Element {
    let busy = use_signal(|| false);
    let saved = format!("Profile {profile} saved");
    let delete = {
        let profile = profile.clone();
        move |()| {
            let profile = profile.clone();
            act(busy, on_done, async move {
                let client = rpc::client().await?;
                client
                    .optimized_outdated_delete(Some(profile))
                    .await
                    .map(outdated_deleted_text)
                    .map_err(rpc::error)
            });
        }
    };
    rsx! {
        DangerDialog {
            title: "{profile} saved",
            text: outdated_message(&profile, n).unwrap_or_default(),
            warning: true,
            cancel: "Keep them for now",
            confirm: "Delete them now",
            busy: busy(),
            on_cancel: move |()| on_done.call(Some(Ok(saved.clone()))),
            on_confirm: delete,
        }
    }
}

/// "Remove Mobile?", saying what happens to its copies.
#[component]
fn RemoveDialog(
    profile: OptimizedProfile,
    on_done: EventHandler<Option<Result<String, String>>>,
) -> Element {
    let busy = use_signal(|| false);
    let name = profile.name.clone();
    let go = move |()| {
        let name = name.clone();
        act(busy, on_done, async move {
            let client = rpc::client().await?;
            let n = client
                .optimized_profile_delete(name.clone())
                .await
                .map_err(rpc::error)?;
            Ok(match n {
                0 => format!("Profile {name} removed"),
                1 => format!("Profile {name} removed; its copy is outdated"),
                n => format!("Profile {name} removed; its {n} copies are outdated"),
            })
        });
    };
    rsx! {
        DangerDialog {
            title: "Remove {profile.name}?",
            text: "Downloads are no longer offered in it. Its copies are outdated: they are not used again, and go first when room is needed, or when you delete them.",
            cancel: "Cancel",
            confirm: "Remove",
            busy: busy(),
            on_cancel: move |()| on_done.call(None),
            on_confirm: go,
        }
    }
}

/// "Delete the copy of Alien?"
#[component]
fn DeleteCopyDialog(
    copy: OptimizedCopy,
    on_done: EventHandler<Option<Result<String, String>>>,
) -> Element {
    let busy = use_signal(|| false);
    let (name, _) = copy_name(&copy);
    let id = copy.id.clone();
    let go = move |()| {
        let id = id.clone();
        act(busy, on_done, async move {
            let client = rpc::client().await?;
            client
                .optimized_copy_delete(id)
                .await
                .map(|d| copy_deleted_text(d).to_owned())
                .map_err(rpc::error)
        });
    };
    let text = if copy.reading {
        "It is being read now: it goes when its reader is done. Nobody starts reading it from now on."
    } else {
        "Its file is removed now. The next download or playback that needs it converts again."
    };
    rsx! {
        DangerDialog {
            title: "Delete the {copy.target} copy of {name}?",
            text,
            cancel: "Cancel",
            confirm: "Delete",
            busy: busy(),
            on_cancel: move |()| on_done.call(None),
            on_confirm: go,
        }
    }
}
