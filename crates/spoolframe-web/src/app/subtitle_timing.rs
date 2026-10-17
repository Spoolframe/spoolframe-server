//! An item's subtitle files and how each is timed (PX-03), on its Debug
//! tab: Sync, Use original timing, and the offset in seconds, each for
//! every user who plays the item. The admin API has no subscriptions, so
//! the card asks again every few seconds while a sync runs.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, SubtitleTimingInfo};

use super::admin::{Said, Toast, say, sleep};
use super::{Alert, icons, rpc};
use crate::logic::ApiError;
use crate::subtitle_timing::{any_syncing, has_sync, offset_by, offset_ms, seconds, sync_text};

/// How often the card asks while a sync runs.
const POLL_MS: i32 = 3_000;

async fn load(id: String) -> Result<Vec<SubtitleTimingInfo>, ApiError> {
    let client = rpc::client().await?;
    client.subtitle_timings(id).await.map_err(rpc::error)
}

/// What a button asks of a track.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    Sync,
    Original,
    Offset(i64),
}

async fn change(id: String, track: u32, change: Change) -> Result<SubtitleTimingInfo, ApiError> {
    let client = rpc::client().await?;
    match change {
        Change::Sync => client.subtitle_sync(id, track).await,
        Change::Original => client.subtitle_use_original(id, track).await,
        Change::Offset(ms) => client.subtitle_set_offset(id, track, ms).await,
    }
    .map_err(rpc::error)
}

#[component]
pub(super) fn SubtitleTimingCard(id: String) -> Element {
    let mut timings = use_resource(use_reactive((&id,), |(id,)| load(id)));
    let toast = use_signal(|| None::<Said>);
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
    let current = timings.read().clone();
    let rows = match current {
        None => return rsx! {},
        Some(Err(e)) => return rsx! { Alert { text: e.explain() } },
        Some(Ok(rows)) => rows,
    };
    if rows.is_empty() {
        return rsx! {};
    }
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 { "Subtitle timing" }
                span { class: "text-xs text-ctp-subtext0", "For everyone who plays it" }
            }
            div { class: "dbg-rows",
                for row in rows.iter() {
                    TimingRow {
                        key: "{row.track}",
                        id: id.clone(),
                        row: row.clone(),
                        on_change: move |_| timings.restart(),
                        toast,
                    }
                }
            }
            Toast { said: toast }
        }
    }
}

#[component]
fn TimingRow(
    id: String,
    row: SubtitleTimingInfo,
    on_change: EventHandler<()>,
    toast: Signal<Option<Said>>,
) -> Element {
    let mut offset = use_signal(|| seconds(row.offset_ms));
    let mut busy = use_signal(|| false);
    let (state, tone) = sync_text(&row.sync);
    let track = row.track;
    let act = use_callback(move |what: Change| {
        let id = id.clone();
        busy.set(true);
        spawn(async move {
            match change(id, track, what).await {
                Ok(after) => {
                    offset.set(seconds(after.offset_ms));
                    let said = match what {
                        Change::Sync => "Syncing; the track plays as it was until it is done",
                        Change::Original => "Original timing, for everyone",
                        Change::Offset(0) => "Offset reset",
                        Change::Offset(_) => "Offset set, for everyone",
                    };
                    say(toast, true, said.into());
                }
                Err(e) => say(toast, false, e.explain()),
            }
            busy.set(false);
            on_change.call(());
        });
    });
    let typed = offset_ms(&offset());
    rsx! {
        div { class: "dbg-row flex-wrap gap-2",
            span { class: "flex min-w-0 flex-col",
                span { "Subtitle {row.track} · {row.label}" }
                span { class: "{tone} text-xs", "{state}" }
                if let Some(by) = offset_by(&row) {
                    span { class: "cat-quiet text-xs", "Offset {seconds(row.offset_ms)} s, {by}" }
                }
            }
            span { class: "flex flex-wrap items-center gap-2",
                label { class: "flex items-center gap-1 text-sm",
                    "Offset"
                    input {
                        class: "pref-select w-20",
                        r#type: "number",
                        step: "0.1",
                        value: "{offset}",
                        oninput: move |e: FormEvent| offset.set(e.value()),
                    }
                    "s"
                }
                button {
                    class: "btn h-[30px]",
                    disabled: busy() || typed.is_none() || typed == Some(row.offset_ms),
                    onclick: move |_| {
                        if let Some(ms) = typed {
                            act.call(Change::Offset(ms));
                        }
                    },
                    icons::Check {}
                    "Set"
                }
                if row.offset_ms != 0 {
                    button {
                        class: "btn h-[30px]",
                        disabled: busy(),
                        onclick: move |_| act.call(Change::Offset(0)),
                        icons::RotateCcw { size: 14 }
                        "Reset"
                    }
                }
                button {
                    class: "btn h-[30px]",
                    disabled: busy() || row.sync == spoolframe_admin_api::SubtitleSyncState::Syncing,
                    onclick: move |_| act.call(Change::Sync),
                    icons::Captions {}
                    "Sync"
                }
                if has_sync(&row.sync) {
                    button {
                        class: "btn h-[30px]",
                        disabled: busy(),
                        onclick: move |_| act.call(Change::Original),
                        icons::History {}
                        "Use original timing"
                    }
                }
            }
        }
    }
}
