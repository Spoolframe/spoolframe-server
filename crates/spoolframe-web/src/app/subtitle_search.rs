//! Subtitles from OpenSubtitles and SubDL on demand (PX-02), the web UI's
//! part of rollout stage 1: on a film's or an episode's page, its
//! downloaded subtitles (who downloaded each, its sync, Remove) and "Find
//! subtitles…" with the results, each marked with its site; Settings ›
//! Subtitles, the sites searched, their keys and the day's downloads; and
//! on a user's page,
//! whether they may download.

use dioxus::prelude::*;
use spoolframe_admin_api::{
    AdminApiClient, Settings, SubtitleResult, SubtitleSearch, SubtitleSource, SubtitleSyncState,
    SubtitleTimingInfo,
};

use super::admin::{Said, Toast, say, sleep};
use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::wizard::{Footer, Mode, Note, entry, load_settings, loading, locked, save, text, value};
use super::{icons, rpc};
use crate::logic::ApiError;
use crate::subtitle_search::{
    LIMIT, details, downloaded_by, downloaded_line, downloads, languages_value, limit_value, marks,
    refusal, secret_reference, site_usage_line, source_lines, sources_on, sources_value,
    user_usage,
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

/// Settings › Subtitles: which subtitle sites are searched, the daily
/// limit per user, OpenSubtitles' account and languages (its API key is
/// built in), and SubDL's key and languages. A key and a password are the
/// names of the server's environment variables that hold them, never the
/// secrets.
#[component]
pub fn SubtitleSitesForm(on_done: EventHandler<()>) -> Element {
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
            SubtitleSiteFields {
                key: "{g}",
                settings: loaded.clone(),
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

const SOURCES_KEY: &str = "subtitles.sources";
const USERNAME: &str = "opensubtitles.username";
const PASSWORD: &str = "opensubtitles.password";
const LANGUAGES: &str = "opensubtitles.languages";
const SUBDL_KEY: &str = "subdl.api_key";
const SUBDL_LANGUAGES: &str = "subdl.languages";

#[component]
fn SubtitleSiteFields(
    settings: Settings,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let s = &settings;
    let password_set = entry(s, PASSWORD).is_some_and(|e| e.set);
    let subdl_key_set = entry(s, SUBDL_KEY).is_some_and(|e| e.set);
    let sources_before = sources_on(value(s, SOURCES_KEY));
    let sources_setting = value(s, SOURCES_KEY).cloned();
    let mut sources = use_signal(|| sources_before.clone());
    let username = use_signal(|| text(s, USERNAME));
    let password = use_signal(String::new);
    let list = |key| {
        value(s, key)
            .map(super::wizard::strings)
            .unwrap_or_default()
            .join(", ")
    };
    let languages = use_signal(|| list(LANGUAGES));
    let subdl_key = use_signal(String::new);
    let subdl_languages = use_signal(|| list(SUBDL_LANGUAGES));
    let limit = use_signal(|| text(s, LIMIT));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // The keys and the account are sensitive: from an old session,
    // "Confirm it's you" first, then the save again.
    let confirming = use_signal(|| None::<String>);
    let saved = settings.clone();
    let on_save = use_callback(move |()| {
        let s = saved.clone();
        let changes = (|| -> Result<Vec<(&'static str, Option<serde_json::Value>)>, String> {
            let mut changes = Vec::new();
            // Only when a box changed: the order the admin gave is kept,
            // and an unset list keeps following the default.
            if let Some(list) = sources_value(sources_setting.as_ref(), &sources_before, &sources())
            {
                changes.push((SOURCES_KEY, Some(list)));
            }
            // A secret's field left empty keeps the one set.
            if let Some(pw) =
                secret_reference("Password", &password(), "SF_ENV_OPENSUBTITLES_PASSWORD")?
            {
                changes.push((PASSWORD, Some(pw)));
            }
            if let Some(key) =
                secret_reference("SubDL's API key", &subdl_key(), "SF_ENV_SUBDL_KEY")?
            {
                changes.push((SUBDL_KEY, Some(key)));
            }
            changes.push((USERNAME, super::wizard::typed(&username())));
            changes.push((LANGUAGES, languages_value(&languages())));
            changes.push((SUBDL_LANGUAGES, languages_value(&subdl_languages())));
            changes.push((LIMIT, limit_value(&limit())?));
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
                Err(e) if asks_to_confirm(&e, confirming, "changing the subtitle sites") => {}
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
                Err(e) if asks_to_confirm(&e, confirming, "changing the subtitle sites") => {}
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };
    let set_hint = "Set. Type another to replace it.";
    let password_hint = if password_set {
        set_hint
    } else {
        "${SF_ENV_OPENSUBTITLES_PASSWORD}"
    };
    let subdl_hint = if subdl_key_set {
        set_hint
    } else {
        "${SF_ENV_SUBDL_KEY}"
    };
    rsx! {
        p { class: "m-0 text-sm text-ctp-subtext0",
            "Subtitles are searched and downloaded from OpenSubtitles, SubDL or both only when someone asks, on an item's page or in an app, and kept for everyone. "
            "Searching sends each site the item's IDs, and OpenSubtitles the file's hash. OpenSubtitles works with Spoolframe's built-in key; SubDL needs its own."
        }
        UsageToday {}
        fieldset { class: "flex flex-col gap-1.5 border-0 p-0 m-0",
            legend { class: "mb-1 text-sm font-semibold", "Sites searched" }
            for site in SubtitleSource::ALL {
                label { key: "{site.id()}", class: "flex items-center gap-2 text-sm",
                    input {
                        r#type: "checkbox",
                        checked: sources().contains(&site),
                        disabled: locked(s, SOURCES_KEY),
                        onchange: move |e| {
                            if e.checked() {
                                sources.write().insert(site);
                            } else {
                                sources.write().remove(&site);
                            }
                        },
                    }
                    "{site.name()}"
                }
            }
            span { class: "text-xs text-ctp-subtext0", "Both at once, as one list. Viewers do not choose." }
            Note { settings: settings.clone(), setting: SOURCES_KEY, on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "Downloads a day per user", value: limit, kind: "number", placeholder: "0", disabled: locked(s, LIMIT) }
            span { class: "text-xs text-ctp-subtext0", "From every site. 0 for no limit. An admin can turn downloads off for a user on their page." }
            Note { settings: settings.clone(), setting: LIMIT, on_changed: move |_| on_reload.call(()) }
        }
        h3 { class: "m-0 text-sm font-semibold", "OpenSubtitles" }
        div { class: "flex flex-col gap-1.5",
            span { class: "text-sm", "API key" }
            span { class: "text-xs text-ctp-subtext0", "Built-in key: Spoolframe's own, nothing to set." }
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
        h3 { class: "m-0 text-sm font-semibold", "SubDL" }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "API key", value: subdl_key, placeholder: subdl_hint, disabled: locked(s, SUBDL_KEY) }
            span { class: "text-xs text-ctp-subtext0",
                "From your account's panel at subdl.com: the server's environment variable that holds it, as ${{SF_ENV_NAME}}, never the key itself, as the settings are kept as plain text. "
                if subdl_key_set {
                    button { class: "btn-plain", onclick: move |_| forget(SUBDL_KEY), "Take it out" }
                }
            }
            Note { settings: settings.clone(), setting: SUBDL_KEY, on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-1.5",
            super::Field { label: "Languages", value: subdl_languages, placeholder: "en", disabled: locked(s, SUBDL_LANGUAGES) }
            Note { settings: settings.clone(), setting: SUBDL_LANGUAGES, on_changed: move |_| on_reload.call(()) }
        }
        if let Some(text) = error() {
            super::Alert { text }
        }
        Footer { mode: Mode::Settings, step: "subtitles", busy: busy(), on_save, on_discard: move |_| on_reload.call(()) }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
    }
}

/// Settings › Subtitles: the day's downloads, each site's (with its
/// account's count as the site said it) and each user's against the
/// daily limit. Nothing without playback to look at.
#[component]
fn UsageToday() -> Element {
    let usage = use_resource(|| async move {
        let client = rpc::client().await?;
        client.subtitle_usage().await.map_err(rpc::error)
    });
    let Some(Ok(usage)) = &*usage.read() else {
        return rsx! {};
    };
    let now = super::now();
    let limit = usage.daily_limit_per_user;
    rsx! {
        div { class: "flex flex-col gap-1.5",
            h3 { class: "m-0 text-sm font-semibold", "Today" }
            span { class: "text-xs text-ctp-subtext0", "The last 24 hours, as the daily limit counts them. A removed download still counts." }
            for line in usage.sites.iter().map(|s| site_usage_line(s, now)) {
                p { key: "{line}", class: "m-0 text-sm", "{line}" }
            }
            if usage.users.is_empty() {
                p { class: "m-0 text-sm text-ctp-subtext0", "No one downloaded a subtitle." }
            } else {
                div { class: "dbg-rows",
                    for (i, user) in usage.users.iter().enumerate() {
                        div { key: "{i}", class: "dbg-row",
                            span { class: "cat-break", "{user.name}" }
                            span { class: "cat-quiet text-xs", "{user_usage(user, limit)}" }
                        }
                    }
                }
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
