//! Subtitles from OpenSubtitles on demand (PX-02), the web UI's part of
//! rollout stage 1: on a film's or an episode's page, its downloaded
//! subtitles (who downloaded each, its sync, Remove) and "Find
//! subtitles…" with the results; Settings › Subtitles, the key and the
//! account; and on a user's page, whether they may download.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, Settings, SubtitleResult, SubtitleSearch, SubtitleSyncState, SubtitleTimingInfo,
};

use super::admin::{Said, Toast, say, sleep};
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::wizard::{
    Footer, Mode, Note, entry, load_settings, loading, locked, number, save, text, value,
};
use super::{icons, rpc};
use crate::logic::ApiError;
use crate::subtitle_search::{
    details, downloaded_by, downloaded_line, downloads, languages_value, marks, refusal,
    secret_reference,
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
                span { class: "text-xs text-ctp-subtext0", "From OpenSubtitles, for everyone" }
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
                "Searching sends OpenSubtitles this file's hash and the item's IDs. A download counts against the server's account."
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
                if found.results.is_empty() {
                    p { class: "m-0 text-sm text-ctp-subtext0", "Nothing found in {found.languages.join(\", \")}." }
                }
                div { class: "dbg-rows",
                    for result in found.results.iter() {
                        ResultRow {
                            key: "{result.file_id}",
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
    let file_id = result.file_id;
    let get = move |_| {
        let id = id.clone();
        busy.set(true);
        spawn(async move {
            let got = async {
                let client = rpc::client().await?;
                client
                    .subtitle_download(id, file_id)
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

/// Settings › Subtitles: OpenSubtitles' key, account, languages and the
/// daily limit per user. The key and the password are the names of the
/// server's environment variables that hold them, never the secrets.
#[component]
pub fn OpenSubtitlesForm(on_done: EventHandler<()>) -> Element {
    let mut generation = use_signal(|| 0u32);
    let settings = use_resource(move || async move {
        let _ = generation();
        load_settings().await
    });
    let current = settings.read().clone();
    if let Some(wait) = loading(&current) {
        return wait;
    }
    let Some(Ok(loaded)) = current else {
        return rsx! {};
    };
    rsx! {
        for g in [generation()] {
            OpenSubtitlesFields {
                key: "{g}",
                settings: loaded.clone(),
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

const API_KEY: &str = "opensubtitles.api_key";
const USERNAME: &str = "opensubtitles.username";
const PASSWORD: &str = "opensubtitles.password";
const LANGUAGES: &str = "opensubtitles.languages";
const LIMIT: &str = "opensubtitles.daily_limit_per_user";

#[component]
fn OpenSubtitlesFields(
    settings: Settings,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let s = &settings;
    let key_set = entry(s, API_KEY).is_some_and(|e| e.set);
    let password_set = entry(s, PASSWORD).is_some_and(|e| e.set);
    let api_key = use_signal(String::new);
    let username = use_signal(|| text(s, USERNAME));
    let password = use_signal(String::new);
    let languages = use_signal(|| {
        value(s, LANGUAGES)
            .map(super::wizard::strings)
            .unwrap_or_default()
            .join(", ")
    });
    let limit = use_signal(|| text(s, LIMIT));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // The key and the account are sensitive: from an old session,
    // "Confirm it's you" first, then the save again.
    let confirming = use_signal(|| None::<String>);
    let saved = settings.clone();
    let on_save = use_callback(move |()| {
        let s = saved.clone();
        let changes = (|| -> Result<Vec<(&'static str, Option<serde_json::Value>)>, String> {
            let mut changes = Vec::new();
            // A secret's field left empty keeps the one set.
            if let Some(key) = secret_reference("API key", &api_key())? {
                changes.push((API_KEY, Some(key)));
            }
            if let Some(pw) = secret_reference("Password", &password())? {
                changes.push((PASSWORD, Some(pw)));
            }
            changes.push((USERNAME, super::wizard::typed(&username())));
            changes.push((LANGUAGES, languages_value(&languages())));
            changes.push((LIMIT, number("The daily limit", &limit())?));
            Ok(changes)
        })();
        let changes = match changes {
            Ok(changes) => changes,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        busy.set(true);
        spawn(async move {
            match save(&s, changes).await {
                Ok(()) => {
                    error.set(None);
                    on_done.call(());
                }
                Err(e) if asks_to_confirm(&e, confirming, "changing OpenSubtitles") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let forget = move |key: &'static str| {
        spawn(async move {
            let unset = async {
                let client = rpc::client().await?;
                client
                    .setting_unset(key.to_owned())
                    .await
                    .map_err(rpc::error)
            };
            match unset.await {
                Ok(_) => on_reload.call(()),
                Err(e) if asks_to_confirm(&e, confirming, "changing OpenSubtitles") => {}
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };
    let key_hint = if key_set {
        "Set. Type another to replace it."
    } else {
        "${SF_ENV_OPENSUBTITLES_KEY}"
    };
    let password_hint = if password_set {
        "Set. Type another to replace it."
    } else {
        "${SF_ENV_OPENSUBTITLES_PASSWORD}"
    };
    rsx! {
        p { class: "m-0 text-sm text-ctp-subtext0",
            "Subtitles are searched and downloaded from OpenSubtitles only when someone asks, on an item's page or in an app, and kept for everyone. "
            "Searching sends OpenSubtitles the file's hash and the item's IDs. Without an API key it is off."
        }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "API key", value: api_key, placeholder: key_hint, disabled: locked(s, API_KEY) }
            span { class: "text-xs text-ctp-subtext0",
                "The server's environment variable that holds the key, as ${{SF_ENV_NAME}}, never the key itself: the settings are kept as plain text. "
                if key_set {
                    button { class: "btn-plain", onclick: move |_| forget(API_KEY), "Take it out" }
                }
            }
            Note { settings: settings.clone(), setting: API_KEY, on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "Account", value: username, placeholder: "Your username at opensubtitles.com", disabled: locked(s, USERNAME) }
            span { class: "text-xs text-ctp-subtext0", "Downloads count against it. Without one, OpenSubtitles allows a handful a day for the server's address." }
            Note { settings: settings.clone(), setting: USERNAME, on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "Password", value: password, placeholder: password_hint, disabled: locked(s, PASSWORD) }
            if password_set {
                span { class: "text-xs text-ctp-subtext0",
                    button { class: "btn-plain", onclick: move |_| forget(PASSWORD), "Take it out" }
                }
            }
            Note { settings: settings.clone(), setting: PASSWORD, on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "Languages", value: languages, placeholder: "en", disabled: locked(s, LANGUAGES) }
            span { class: "text-xs text-ctp-subtext0", "Searched first, in order: en, pt-BR." }
            Note { settings: settings.clone(), setting: LANGUAGES, on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "Downloads a day per user", value: limit, kind: "number", placeholder: "0", disabled: locked(s, LIMIT) }
            span { class: "text-xs text-ctp-subtext0", "0 for no limit. An admin can turn downloads off for a user on their page." }
            Note { settings: settings.clone(), setting: LIMIT, on_changed: move |_| on_reload.call(()) }
        }
        if let Some(text) = error() {
            super::Alert { text }
        }
        Footer { mode: Mode::Settings, step: "subtitles", busy: busy(), on_save, on_discard: move |_| on_reload.call(()) }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
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
                    "May download subtitles from OpenSubtitles"
                }
                span { class: "note", "Searching stays theirs either way." }
            }
        }
    }
}
