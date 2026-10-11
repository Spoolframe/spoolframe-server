//! The admin's Overview (P3-02), the admin's start: what plays now, who
//! uses the server, how long it has run, whether Plex answers, and the
//! libraries, from the APIs the Server pages already use. Laid out as
//! WebAdminPhone 4 draws it at phone width (four tiles over "Playing
//! now"), and as two columns of cards on a wide screen.

use dioxus::prelude::*;
use spoolframe_admin_api::AdminApiClient;

use super::admin::{CopyButton, sleep};
use super::{Route, icons, rpc};
use crate::activity::uptime;
use crate::libraries::{PathCell, down, items_line, path_cell};
use crate::overview::playing_lines;

/// How often the page asks again: what plays changes by the minute.
const REFRESH_MS: i32 = 15_000;

#[component]
pub fn AdminHome() -> Element {
    // Each refresh bumps this, and every card reads again.
    let mut tick = use_signal(|| 0u32);
    use_hook(move || {
        spawn(async move {
            loop {
                sleep(REFRESH_MS).await;
                tick += 1;
            }
        });
    });
    let status = use_resource(|| async {
        let client = rpc::client().await?;
        client.server_status().await.map_err(rpc::error)
    });
    let users = use_resource(move || async move {
        let _ = tick();
        let client = rpc::client().await?;
        client.user_list().await.map_err(rpc::error)
    });
    let activity = use_resource(move || async move {
        let _ = tick();
        let client = rpc::client().await?;
        client.activity().await.map_err(rpc::error)
    });
    let playbacks = use_resource(move || async move {
        let _ = tick();
        let client = rpc::client().await?;
        client.playbacks().await.map_err(rpc::error)
    });
    let plex = use_resource(move || async move {
        let _ = tick();
        let client = rpc::client().await?;
        client.plex_status().await.map_err(rpc::error)
    });
    let libraries = use_resource(move || async move {
        let _ = tick();
        let client = rpc::client().await?;
        client.libraries().await.map_err(rpc::error)
    });
    let playing = playbacks.read().clone().map(|r| r.map(|list| list.open));
    let playing_count = match &playing {
        Some(Ok(open)) => open.len().to_string(),
        _ => "—".to_owned(),
    };
    let user_count = match &*users.read() {
        Some(Ok(list)) => list.len().to_string(),
        _ => "—".to_owned(),
    };
    let up = match &*activity.read() {
        Some(Ok(a)) => uptime(a.totals.uptime_secs),
        _ => "—".to_owned(),
    };
    let (plex_word, plex_ok) = match &*plex.read() {
        Some(Ok(p)) if p.reachable => ("Reachable", true),
        Some(Ok(_)) => ("Can't be reached", false),
        Some(Err(_)) => ("Unknown", false),
        None => ("—", true),
    };
    rsx! {
        div { class: "flex flex-col gap-[22px] max-md:gap-3.5",
            div { class: "page-head hidden md:flex",
                h1 { class: "page-title page-title-admin", "Overview" }
            }
            div { class: "ov-tiles",
                Tile { label: "Playing now", value: playing_count, to: Route::PlaybackPage {} }
                Tile { label: "Users", value: user_count, to: Route::Users {} }
                Tile { label: "Uptime", value: up.clone(), to: Route::ActivityPage {} }
                Tile {
                    label: "Plex",
                    value: plex_word,
                    bad: !plex_ok,
                    to: Route::Settings { tab: "plex".into() },
                }
            }
            div { class: "ov-columns",
                section { class: "card",
                    div { class: "card-head",
                        icons::Play { size: 18 }
                        h2 { "Playing now" }
                        Link { class: "ml-auto text-[13px]", to: Route::PlaybackPage {}, "Playback" }
                    }
                    div { class: "card-body",
                        match playing {
                            None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                            Some(Err(e)) => rsx! { super::Alert { text: e.explain() } },
                            Some(Ok(open)) if open.is_empty() => rsx! {
                                p { class: "m-0 text-sm text-ctp-subtext0", "Nothing is playing." }
                            },
                            Some(Ok(open)) => rsx! {
                                for row in open {
                                    {
                                        let (what, where_) = playing_lines(&row);
                                        rsx! {
                                            Link { key: "{row.id}", class: "ov-playing", to: Route::PlaybackDetail { id: row.id.clone() },
                                                b { "{what}" }
                                                span { "{where_}" }
                                            }
                                        }
                                    }
                                }
                            },
                        }
                    }
                }
                div { class: "flex flex-col gap-4",
                    section { class: "card",
                        div { class: "card-head",
                            icons::Server {}
                            h2 { "This server" }
                            Link { class: "ml-auto text-[13px]", to: Route::Server {}, "Server" }
                        }
                        div { class: "card-body",
                            match &*status.read() {
                                None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                                Some(Err(e)) => rsx! { super::Alert { text: e.explain() } },
                                Some(Ok(s)) => rsx! {
                                    b { "{s.name}" }
                                    if let Some(fingerprint) = &s.fingerprint {
                                        span { class: "fingerprint", "{fingerprint}" }
                                    }
                                    span { class: "text-[13px] text-ctp-subtext0", "Version {s.version} · up {up}" }
                                    if let Some(key) = s.endpoint_id.clone() {
                                        div { CopyButton { text: key, label: "Copy server key" } }
                                    }
                                },
                            }
                        }
                    }
                    section { class: "card",
                        div { class: "card-head",
                            icons::Film {}
                            h2 { "Libraries" }
                            Link { class: "ml-auto text-[13px]", to: Route::Libraries {}, "Libraries" }
                        }
                        div { class: "card-body",
                            match &*libraries.read() {
                                None => rsx! { p { class: "m-0 text-sm text-ctp-subtext0", "Loading…" } },
                                Some(Err(e)) => rsx! { super::Alert { text: e.explain() } },
                                Some(Ok(o)) if o.libraries.is_empty() => rsx! {
                                    p { class: "m-0 text-sm text-ctp-subtext0", "No libraries yet." }
                                },
                                Some(Ok(o)) => rsx! {
                                    for l in o.libraries.iter() {
                                        div { key: "{l.id}", class: "row",
                                            span { class: "row-what",
                                                b { "{l.name}" }
                                                span { class: "row-sub", "{items_line(l.kind, l.items)} · {l.server_name}" }
                                            }
                                            // As Libraries' Paths column says it.
                                            span { class: "row-do",
                                                match path_cell(l, !down(o).iter().any(|(server, _)| *server == l.server_name)) {
                                                    PathCell::Ok => rsx! { span { class: "state state-ok", "OK" } },
                                                    PathCell::Missing(n) => rsx! { span { class: "state state-warn", "{n} missing" } },
                                                    PathCell::Unavailable(_) => rsx! { span { class: "state state-bad", "Unavailable" } },
                                                    PathCell::Unknown => rsx! { span { class: "text-ctp-subtext0", "—" } },
                                                }
                                            }
                                        }
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

/// One of the four numbers at the top, which opens its page.
#[component]
fn Tile(label: String, value: String, #[props(default)] bad: bool, to: Route) -> Element {
    rsx! {
        Link { class: "ov-tile", to,
            span { "{label}" }
            b { class: if bad { "text-ctp-red" } else { "" }, "{value}" }
        }
    }
}
