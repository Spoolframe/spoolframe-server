//! Server › Settings › Experimental: every switch that may change or go
//! away in one place. HTTP playback for AirPlay (PX-49), which was the
//! config file's and the CLI's only, libplacebo's shared Vulkan device on
//! AMD (PGPU-04), which was at the foot of Playback, and keyframes read
//! ahead of any play (`experimental.index_ahead`).

use dioxus::prelude::*;
use serde_json::json;
use spoolframe_admin_api::Settings;

use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::wizard::{Footer, Mode, Note, flag, load_settings, loading, locked, save, text, typed};
use super::{Alert, Field, Notice};
use crate::experimental::Listen;
use crate::setup;

#[component]
pub fn ExperimentalForm(on_done: EventHandler<()>) -> Element {
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
            ExperimentalFields {
                key: "{g}",
                settings: loaded.clone(),
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

#[component]
fn ExperimentalFields(
    settings: Settings,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let listen = Listen::of(&text(&settings, "http_playback.listen"));
    let mut airplay = use_signal(|| listen.on());
    let port = use_signal(|| listen.port_text());
    let address = use_signal(|| text(&settings, "http_playback.address"));
    let mut shared_on_amd = use_signal(|| flag(&settings, "experimental.libplacebo_shared_on_amd"));
    let mut index_ahead = use_signal(|| flag(&settings, "experimental.index_ahead"));
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // `http_playback.*` is sensitive: from an old session, "Confirm it's
    // you" first, then the save again.
    let confirming = use_signal(|| None::<String>);
    let s = settings.clone();
    let shown = listen.clone();
    let on_save = use_callback(move |()| {
        let s = s.clone();
        let listen = match shown.setting(airplay(), &port()) {
            Ok(listen) => listen.map(serde_json::Value::String),
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        let changes = vec![
            ("http_playback.listen", listen),
            ("http_playback.address", typed(&address())),
            (
                "experimental.libplacebo_shared_on_amd",
                Some(json!(shared_on_amd())),
            ),
            ("experimental.index_ahead", Some(json!(index_ahead()))),
        ];
        busy.set(true);
        spawn(async move {
            match save(&s, changes).await {
                Ok(()) => on_done.call(()),
                Err(e) if asks_to_confirm(&e, confirming, "changing HTTP playback") => {}
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    });
    let listen_locked = locked(&settings, "http_playback.listen");
    rsx! {
        p { class: "m-0 text-sm text-ctp-subtext0", {setup::EXPERIMENTAL_NOTE} }
        div { class: "flex flex-col gap-2",
            h2 { "AirPlay" }
            div { class: "flex items-start gap-3",
                button {
                    class: "switch mt-0.5",
                    role: "switch",
                    aria_checked: "{airplay()}",
                    aria_label: "HTTP playback for AirPlay",
                    disabled: listen_locked,
                    onclick: move |_| airplay.toggle(),
                }
                span { class: "choice-what",
                    b { "HTTP playback for AirPlay" }
                    "A playback served over plain HTTP on a port of its own, so a phone can AirPlay it to an Apple TV. Apps use it only when their own experimental HTTP playback is turned on too."
                }
            }
            Note { settings: settings.clone(), setting: "http_playback.listen", on_changed: move |_| on_reload.call(()) }
            if let Listen::Other(other) = &listen {
                span { class: "text-xs text-ctp-subtext0",
                    "Listens at "
                    span { class: "mono-value text-xs", "{other}" }
                    ". Changing the port or the switch replaces it."
                }
            }
            Notice {
                "Plain HTTP, with no sign-in of its own: each URL carries a token for one playback. Open the port to your home network only."
            }
            if airplay() {
                div { class: "limits",
                    div { class: "flex flex-col gap-1.5",
                        Field { label: "Port", value: port, kind: "number", placeholder: "7422", disabled: listen_locked }
                        span { class: "text-xs text-ctp-subtext0", "Default 7422 · the TCP port, on every address of the server" }
                    }
                }
                div { class: "flex flex-col gap-1.5",
                    Field { label: "Address for apps outside your home", value: address, placeholder: "media.example.org", disabled: locked(&settings, "http_playback.address") }
                    span { class: "text-xs text-ctp-subtext0", "Optional. A host name or an IP address, no port. An app that reaches the server over a relay or the Internet gets a URL only with one; an app at home gets the address it reached the server at." }
                    Note { settings: settings.clone(), setting: "http_playback.address", on_changed: move |_| on_reload.call(()) }
                }
            }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Tone mapping on AMD" }
            div { class: "flex items-start gap-3",
                button {
                    class: "switch mt-0.5",
                    role: "switch",
                    aria_checked: "{shared_on_amd()}",
                    aria_label: "libplacebo shares ffmpeg's Vulkan device on AMD",
                    disabled: locked(&settings, "experimental.libplacebo_shared_on_amd"),
                    onclick: move |_| shared_on_amd.toggle(),
                }
                span { class: "choice-what",
                    b { "libplacebo shares ffmpeg's Vulkan device on AMD" }
                    "The frames stay on the GPU from decode to tone map, rather than going through memory to a device of libplacebo's own. Tried at start; where it fails, as it does with Mesa's RADV today, libplacebo uses its own."
                }
            }
            Note { settings: settings.clone(), setting: "experimental.libplacebo_shared_on_amd", on_changed: move |_| on_reload.call(()) }
        }
        div { class: "flex flex-col gap-2",
            h2 { "Keyframes read ahead" }
            div { class: "flex items-start gap-3",
                button {
                    class: "switch mt-0.5",
                    role: "switch",
                    aria_checked: "{index_ahead()}",
                    aria_label: "Read keyframes ahead of any play",
                    disabled: locked(&settings, "experimental.index_ahead"),
                    onclick: move |_| index_ahead.toggle(),
                }
                span { class: "choice-what",
                    b { "Read keyframes ahead of any play" }
                    "A film in a transport stream (.ts, .m2ts) has no index of its keyframes, so its first playback with the video copied reads the whole file first: minutes for a large remux, longer than an app waits. With this on, the server reads such files before anyone plays them, the newest first, one at a time, at the lowest priority, while nothing converts, and keeps what it read."
                }
            }
            Note { settings: settings.clone(), setting: "experimental.index_ahead", on_changed: move |_| on_reload.call(()) }
        }
        if let Some(text) = error() {
            Alert { text }
        }
        Footer { mode: Mode::Settings, step: "experimental", busy: busy(), on_save, on_discard: move |_| on_reload.call(()) }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
    }
}
