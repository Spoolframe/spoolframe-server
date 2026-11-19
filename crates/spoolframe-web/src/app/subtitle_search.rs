//! Subtitles from OpenSubtitles and SubDL on demand (PX-02), the web UI's
//! part of rollout stage 1: on a film's or an episode's page, its
//! downloaded subtitles (who downloaded each, its sync, Remove) and "Find
//! subtitles…" with the results, each marked with its site; Settings ›
//! Subtitles, the sites searched, their keys and the day's downloads; and
//! on a user's page,
//! whether they may download.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, SubtitleResult, SubtitleSearch, SubtitleSyncState, SubtitleTimingInfo,
};

use super::admin::{Said, Toast, say, sleep};
use super::{icons, rpc};
use crate::logic::ApiError;
use crate::subtitle_search::{
    details, downloaded_by, downloaded_line, downloads, marks, refusal, source_lines,
};
use crate::subtitle_timing::{any_syncing, sync_text};

/// How often the card asks again while a download syncs.
const POLL_MS: i32 = 3_000;

async fn load_timings(id: String) -> Result<Vec<SubtitleTimingInfo>, ApiError> {
    let client = rpc::client().await?;
    client.subtitle_timings(id).await.map_err(rpc::error)
}

async fn search(id: String, language: Option<String>) -> Result<SubtitleSearch, ApiError> {
    let client = rpc::client().await?;
    client
        .subtitle_search(id, language)
        .await
        .map_err(rpc::error)
}

/// A film's or an episode's downloaded subtitles, and finding more.
#[component]
pub(super) fn SubtitlesCard(id: String) -> Element {
    let mut timings = use_resource(use_reactive((&id,), |(id,)| load_timings(id)));
    let toast = use_signal(|| None::<Said>);
    let mut finding = use_signal(|| false);
    use_hook(move || {
        spawn(async move {
            loop {
                sleep(POLL_MS).await;
                let running = matches!(&*timings.peek(), Some(Ok(t)) if any_syncing(t));
                if running {
                    timings.restart();
                }
            }
        });
    });
    let rows = match &*timings.read() {
        Some(Ok(rows)) => downloads(rows),
        _ => Vec::new(),
    };
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "Subtitles" }
                span { class: "text-xs text-ctp-subtext0", "From OpenSubtitles or SubDL, for everyone" }
            }
            div { class: "dbg-rows",
                for row in rows.iter() {
                    DownloadRow {
                        key: "{row.track}",
                        id: id.clone(),
                        row: row.clone(),
                        toast,
                        on_change: move |_| timings.restart(),
                    }
                }
                if rows.is_empty() {
                    p { class: "m-0 text-sm text-ctp-subtext0", "None downloaded." }
                }
            }
            div { class: "card-body",
                if finding() {
                    Finder {
                        id: id.clone(),
                        toast,
                        on_downloaded: move |_| timings.restart(),
                    }
                } else {
                    div {
                        button { class: "btn", onclick: move |_| finding.set(true),
                            icons::Search { size: 14 }
                            "Find subtitles…"
                        }
                    }
                }
            }
            Toast { said: toast }
        }
    }
}

#[component]
fn DownloadRow(
    id: String,
    row: SubtitleTimingInfo,
    toast: Signal<Option<Said>>,
    on_change: EventHandler<()>,
) -> Element {
    let mut busy = use_signal(|| false);
    let (state, tone) = sync_text(&row.sync);
    let Some(download) = row.download.clone() else {
        return rsx! {};
    };
    let remove = move |_| {
        let id = id.clone();
        let download = download.id.clone();
        busy.set(true);
        spawn(async move {
            let removed = async {
                let client = rpc::client().await?;
                client
                    .subtitle_remove(id, download)
                    .await
                    .map_err(rpc::error)
            };
            match removed.await {
                Ok(()) => say(toast, true, "Removed, for everyone".into()),
                Err(e) => say(toast, false, e.explain()),
            }
            busy.set(false);
            on_change.call(());
        });
    };
    let can_remove = row.download.as_ref().is_some_and(|d| d.can_remove);
    rsx! {
        div { class: "dbg-row flex-wrap gap-2",
            span { class: "flex min-w-0 flex-col",
                span { class: "cat-break", "Subtitle {row.track + 1} · {row.label}" }
                span { class: "cat-quiet text-xs", "{downloaded_by(&row)}" }
                if row.sync != SubtitleSyncState::Original {
                    span { class: "{tone} text-xs", "{state}" }
                }
            }
            if can_remove {
                button { class: "btn h-[30px]", disabled: busy(), onclick: remove,
                    icons::Trash {}
                    "Remove"
                }
            }
        }
    }
}

/// The search: a language, then the results, each with Download.
#[component]
fn Finder(id: String, toast: Signal<Option<Said>>, on_downloaded: EventHandler<()>) -> Element {
    let mut language = use_signal(String::new);
    let mut found = use_signal(|| None::<Result<SubtitleSearch, String>>);
    let mut busy = use_signal(|| false);
    let run = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            let typed = language().trim().to_owned();
            busy.set(true);
            spawn(async move {
                let asked = (!typed.is_empty()).then_some(typed);
                found.set(Some(
                    search(id, asked)
                        .await
                        .map_err(|e| refusal(&e, super::now())),
                ));
                busy.set(false);
            });
        }
    };
    let rows = match found() {
        Some(Ok(found)) => Some(found),
        _ => None,
    };
    rsx! {
        div { class: "flex flex-col gap-3",
            p { class: "m-0 text-sm text-ctp-subtext0",
                "Searching sends the subtitle sites set up the item's IDs, and OpenSubtitles this file's hash. A download counts against the server's account at its site."
            }
            div { class: "flex flex-wrap items-center gap-2",
                label { class: "flex items-center gap-1 text-sm",
                    "Language"
                    input {
                        class: "pref-select w-24",
                        placeholder: "as set",
                        value: "{language}",
                        oninput: move |e: FormEvent| language.set(e.value()),
                    }
                }
                button { class: "btn h-[30px]", disabled: busy(), onclick: run,
                    icons::Search { size: 14 }
                    "Search"
                }
            }
            if let Some(Err(why)) = found() {
                p { class: "m-0 text-sm text-ctp-red", "{why}" }
            }
            if let Some(found) = rows {
                for line in source_lines(&found, super::now()) {
                    p { key: "{line}", class: "m-0 text-sm text-ctp-yellow", "{line}" }
                }
                if found.results.is_empty() {
                    p { class: "m-0 text-sm text-ctp-subtext0", "Nothing found in {found.languages.join(\", \")}." }
                }
                div { class: "dbg-rows",
                    for result in found.results.iter() {
                        ResultRow {
                            key: "{result.id}",
                            id: id.clone(),
                            result: result.clone(),
                            toast,
                            on_downloaded: move |_| {
                                on_downloaded.call(());
                            },
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ResultRow(
    id: String,
    result: SubtitleResult,
    toast: Signal<Option<Said>>,
    on_downloaded: EventHandler<()>,
) -> Element {
    let mut busy = use_signal(|| false);
    let mut done = use_signal(|| result.downloaded);
    let result_id = result.id.clone();
    let get = move |_| {
        let id = id.clone();
        let result_id = result_id.clone();
        busy.set(true);
        spawn(async move {
            let got = async {
                let client = rpc::client().await?;
                client
                    .subtitle_download(id, result_id)
                    .await
                    .map_err(rpc::error)
            };
            match got.await {
                Ok(got) => {
                    done.set(true);
                    say(toast, true, downloaded_line(&got));
                }
                Err(e) => say(toast, false, refusal(&e, super::now())),
            }
            busy.set(false);
            on_downloaded.call(());
        });
    };
    let release = if result.release.is_empty() {
        "(no release name)".to_owned()
    } else {
        result.release.clone()
    };
    rsx! {
        div { class: "dbg-row flex-wrap gap-2",
            span { class: "flex min-w-0 flex-col gap-0.5",
                span { class: "cat-break", "{release}" }
                span { class: "cat-quiet text-xs", "{details(&result)}" }
                span { class: "flex flex-wrap gap-1",
                    for (mark, class) in marks(&result) {
                        span { key: "{mark}", class: "{class}", "{mark}" }
                    }
                }
            }
            button { class: "btn h-[30px]", disabled: busy() || done(), onclick: get,
                icons::Download { size: 14 }
                if done() { "Downloaded" } else { "Download" }
            }
        }
    }
}

/// On a user's page: whether they may download subtitles.
#[component]
pub(super) fn UserSubtitleDownloads(
    username: String,
    name: String,
    toast: Signal<Option<Said>>,
) -> Element {
    let mut allowed = use_resource(use_reactive((&username,), |(username,)| async move {
        let client = rpc::client().await?;
        client
            .user_subtitle_downloads(username)
            .await
            .map_err(rpc::error)
    }));
    let Some(Ok(on)) = *allowed.read() else {
        return rsx! {};
    };
    let flip = move |_| {
        let username = username.clone();
        let name = name.clone();
        spawn(async move {
            let set = async {
                let client = rpc::client().await?;
                client
                    .user_set_subtitle_downloads(username, !on)
                    .await
                    .map_err(rpc::error)
            };
            match set.await {
                Ok(()) if on => say(
                    toast,
                    true,
                    format!("{name} can no longer download subtitles"),
                ),
                Ok(()) => say(toast, true, format!("{name} can download subtitles")),
                Err(e) => say(toast, false, e.explain()),
            }
            allowed.restart();
        });
    };
    rsx! {
        section { class: "card",
            div { class: "card-head", h2 { "Subtitles" } }
            div { class: "card-body",
                label { class: "flex items-center gap-2.5 text-sm",
                    button {
                        class: "switch",
                        role: "switch",
                        aria_checked: "{on}",
                        aria_label: "May download subtitles",
                        onclick: flip,
                    }
                    "May download subtitles from the subtitle sites"
                }
                span { class: "note", "Searching stays theirs either way." }
            }
        }
    }
}
