//! Server › Logs (P3-14), as the WebLogs board lays it out: the server's
//! last log lines, filtered by level and text, following new ones until
//! paused, and Download for a bug report. The admin API has no
//! subscriptions, so following is asking again after the server's cursor.

use std::rc::Rc;

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, LogLevel, LogPage, LogQuery};
use wasm_bindgen::{JsCast, JsValue};

use super::admin::sleep;
use super::{Alert, icons, now_ms, rpc};
use crate::logic::ApiError;
use crate::logs::{LEVELS, LogFilter, LogView, clock, download_name, level_name, short_target};

/// How often new lines are asked for.
const POLL_MS: i32 = 2_000;
/// How long typing waits before the filter is asked for.
const TYPING_MS: i32 = 300;
/// The most full pages asked for one after another, so a burst of lines
/// does not keep the page asking.
const MOST_PAGES: usize = 10;

async fn ask(query: LogQuery) -> Result<LogPage, ApiError> {
    let client = rpc::client().await?;
    client.logs(query).await.map_err(rpc::error)
}

/// Ask for what is new, page after page while they come back full.
async fn pull(mut view: Signal<LogView>, mut error: Signal<Option<ApiError>>) {
    for _ in 0..MOST_PAGES {
        let asked = view.peek().query();
        match ask(asked.clone()).await {
            Ok(page) => {
                if error.peek().is_some() {
                    error.set(None);
                }
                // Asked every 2 s: only what changes is drawn again.
                if !view.peek().is_news(&asked, &page) || !view.write().take(&asked, page) {
                    return;
                }
            }
            Err(e) => {
                error.set(Some(e));
                return;
            }
        }
    }
}

/// The browser's local time of Unix milliseconds `at`, as Unix
/// milliseconds moved by its offset from UTC then.
fn local(at: i64) -> i64 {
    let offset_min = js_sys::Date::new(&JsValue::from_f64(at as f64)).get_timezone_offset();
    at - (offset_min * 60_000.0) as i64
}

/// Save `text` as a file called `name`, as a link to it clicked would.
fn save_file(name: &str, text: &str) -> Result<(), JsValue> {
    let parts = js_sys::Array::of1(&JsValue::from_str(text));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("text/plain;charset=utf-8");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let link: web_sys::HtmlAnchorElement = document.create_element("a")?.unchecked_into();
    link.set_href(&url);
    link.set_download(name);
    link.click();
    web_sys::Url::revoke_object_url(&url)
}

#[component]
pub fn LogsPage() -> Element {
    let mut level = use_signal(|| LogLevel::Info);
    let mut typed = use_signal(String::new);
    let mut typing = use_signal(|| 0u64);
    let mut view = use_signal(|| {
        LogView::new(
            LogFilter {
                level: LogLevel::Info,
                text: String::new(),
            },
            true,
        )
    });
    let error = use_signal(|| None::<ApiError>);
    let mut saving = use_signal(|| None::<String>);
    let mut lines_box = use_signal(|| None::<Rc<MountedData>>);
    use_hook(move || {
        spawn(async move {
            loop {
                pull(view, error).await;
                sleep(POLL_MS).await;
            }
        });
    });
    // Following keeps the newest line in sight.
    let shown_count = view.read().shown().len();
    let following = view.read().following();
    use_effect(use_reactive(
        (&shown_count, &following),
        move |(_, following)| {
            if following && let Some(lines) = lines_box.peek().clone() {
                spawn(async move {
                    let bottom = dioxus::html::geometry::PixelsVector2D::new(0.0, 1e9);
                    let _ = lines.scroll(bottom, ScrollBehavior::Instant).await;
                });
            }
        },
    ));
    // Begin again with another filter: the server applies it.
    let mut refilter = move || {
        let following = view.peek().following();
        view.set(LogView::new(
            LogFilter {
                level: level(),
                text: typed(),
            },
            following,
        ));
        spawn(async move { pull(view, error).await });
    };
    let on_type = move |e: FormEvent| {
        typed.set(e.value());
        typing += 1;
        let at = typing();
        spawn(async move {
            sleep(TYPING_MS).await;
            if *typing.peek() == at {
                refilter();
            }
        });
    };
    let toggle = move |_| {
        if view.peek().following() {
            view.write().pause();
        } else {
            view.write().follow();
        }
    };
    let download = move |_| {
        let text = view.peek().download_text();
        if let Err(e) = save_file(&download_name(now_ms()), &text) {
            saving.set(Some(format!("The file could not be made: {e:?}")));
        }
    };
    let current = view.read().clone();
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin hidden md:block", "Logs" }
                div { class: "flex flex-wrap items-center gap-2.5",
                    span { class: "live", role: "status",
                        span { class: if current.following() && error().is_none() { "live-dot" } else { "live-dot live-dot-off" } }
                        "{current.follow_line()}"
                    }
                    button { class: "btn h-[30px]", onclick: toggle,
                        if current.following() {
                            icons::Pause {}
                            "Pause"
                        } else {
                            icons::Play { size: 15 }
                            "Follow"
                        }
                    }
                    button { class: "btn h-[30px]", disabled: current.shown().is_empty(), onclick: download,
                        icons::Download {}
                        "Download"
                    }
                }
            }
            div { class: "flex flex-wrap items-center gap-2.5",
                div { class: "seg", role: "radiogroup", aria_label: "Lowest level shown",
                    for (l, label) in LEVELS {
                        button {
                            key: "{label}",
                            class: if l == level() { "seg-item seg-item-on" } else { "seg-item" },
                            role: "radio",
                            aria_checked: "{l == level()}",
                            onclick: move |_| {
                                level.set(l);
                                refilter();
                            },
                            "{label}"
                        }
                    }
                }
                form { class: "cat-search", role: "search", onsubmit: move |e| e.prevent_default(),
                    icons::Search { size: 16 }
                    input {
                        r#type: "search",
                        aria_label: "Filter text",
                        placeholder: "Filter text, e.g. 7c1e or plex",
                        value: "{typed}",
                        oninput: on_type,
                    }
                }
                span { class: "grow max-md:hidden" }
                span { class: "text-[13px] text-ctp-subtext0", "{current.count_line()}" }
            }
            if let Some(e) = error() {
                Alert { text: format!("Not updating: {}", e.explain()) }
            }
            if let Some(text) = saving() {
                Alert { text }
            }
            section {
                class: "log-box",
                onmounted: move |e| lines_box.set(Some(e.data())),
                if !current.loaded() && error().is_none() {
                    div { class: "log-tail", "Loading…" }
                } else if current.loaded() && current.shown().is_empty() {
                    div { class: "log-tail",
                        if current.filter().text.trim().is_empty() {
                            "No lines at this level yet."
                        } else {
                            "No lines match."
                        }
                    }
                }
                for line in current.shown().iter() {
                    div {
                        key: "{line.seq}",
                        class: match line.level {
                            LogLevel::Error => "log-line log-error",
                            LogLevel::Warn => "log-line log-warn",
                            _ => "log-line",
                        },
                        span { class: "log-time", "{clock(local(line.at))}" }
                        span { class: "log-level log-level-{level_name(line.level).to_lowercase()}", "{level_name(line.level)}" }
                        span { class: "log-target", title: "{line.target}", "{short_target(&line.target)}" }
                        span { class: "log-text", "{line.text}" }
                    }
                }
                if current.missed() {
                    div { class: "log-tail text-ctp-yellow", "Some lines came faster than the server keeps them and were gone before they were shown." }
                }
                if current.loaded() {
                    div { class: "log-tail", "{current.tail_line()}" }
                }
            }
            p { class: "m-0 text-xs text-ctp-subtext0",
                "The server's last lines at its log level, kept in memory (logs.buffer_lines). Secrets and tokens are never logged; this is what the journal shows. Download saves the lines shown, for a bug report."
            }
        }
    }
}
