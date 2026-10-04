//! Server › Settings (P3-10): the wizard's forms after setup, a tab each,
//! as the WebSettings board lays them out. A tab saves only what was
//! changed in it.

use dioxus::prelude::*;

use super::Route;
use super::admin::{Said, Toast, say};
use super::wizard::{
    AccessForm, Mode, PathsForm, PlaybackForm, PlexForm, SecurityForm, ServerForm,
};

/// The tabs, by their path and name.
const TABS: &[(&str, &str)] = &[
    ("server", "Server"),
    ("plex", "Plex"),
    ("paths", "Paths"),
    ("playback", "Playback"),
    ("access", "Access"),
    ("security", "Security"),
];

#[component]
pub fn SettingsHome() -> Element {
    navigator().replace(Route::Settings {
        tab: "server".into(),
    });
    rsx! {}
}

#[component]
pub fn Settings(tab: String) -> Element {
    let toast = use_signal(|| None::<Said>);
    // Each save reads the tab's settings again: what was saved, and where
    // it now comes from.
    let mut saved = use_signal(|| 0u32);
    let on_done = move |_| {
        saved += 1;
        say(toast, true, "Saved".into());
    };
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin", "Settings" }
            }
            nav { class: "tabs", aria_label: "Settings",
                for (path, name) in TABS {
                    Link {
                        key: "{path}",
                        class: if *path == tab { "tab tab-here" } else { "tab" },
                        to: Route::Settings { tab: (*path).to_owned() },
                        "{name}"
                    }
                }
            }
            section { class: "card max-w-[800px]",
                div { class: "card-body form gap-[18px]",
                    for n in [saved()] {
                        match tab.as_str() {
                            "server" => rsx! { ServerForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "plex" => rsx! { PlexForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "paths" => rsx! { PathsForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "playback" => rsx! { PlaybackForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "access" => rsx! { AccessForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "security" => rsx! { SecurityForm { key: "{n}" } },
                            _ => rsx! { p { key: "{n}", "There is no such tab." } },
                        }
                    }
                }
            }
        }
        Toast { said: toast }
    }
}
