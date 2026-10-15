//! Server › Settings (P3-10): the wizard's forms after setup, a tab each,
//! as the WebSettings board lays them out. A tab saves only what was
//! changed in it.

use dioxus::prelude::*;

use super::Route;
use super::admin::{Said, Toast, say};
use super::experimental::ExperimentalForm;
use super::remote::RemoteAccessForm;
use super::restart::PendingRestart;
use super::sso::SsoForm;
use super::telemetry::TelemetryForm;
use super::wizard::{
    AccessForm, MediaToolsForm, Mode, PathsForm, PlaybackForm, PlexForm, SecurityForm, ServerForm,
    load_settings, value,
};
use crate::advanced;

/// The tabs, by their path and name.
const TABS: &[(&str, &str)] = &[
    ("server", "Server"),
    ("plex", "Plex"),
    ("paths", "Paths"),
    ("playback", "Playback"),
    ("access", "Access"),
    ("remote", "Remote access"),
    ("security", "Security"),
    ("sso", "Sign-in providers"),
    ("telemetry", "Telemetry"),
    // Always shown, not with the advanced settings only: AirPlay is
    // looked for.
    ("experimental", "Experimental"),
];

/// "Show advanced settings" (PX-22), for the forms under the tabs: off in
/// the wizard, which has no switch.
#[derive(Clone, Copy)]
pub(super) struct Advanced(pub Signal<bool>);

/// Whether the advanced settings are shown. A hook: call it where hooks
/// are called.
pub(super) fn show_advanced() -> bool {
    try_use_context::<Advanced>().is_some_and(|Advanced(on)| on())
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// The switch as this browser keeps it; off when storage fails.
fn kept_advanced() -> bool {
    let kept = storage().and_then(|s| s.get_item(advanced::STORAGE_KEY).ok().flatten());
    advanced::kept(kept.as_deref())
}

fn keep_advanced(on: bool) {
    if let Some(s) = storage() {
        let _ = s.set_item(advanced::STORAGE_KEY, advanced::keep(on));
    }
}

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
    let mut advanced_on = use_signal(kept_advanced);
    use_context_provider(|| Advanced(advanced_on));
    let mut set_advanced = move |on: bool| {
        advanced_on.set(on);
        keep_advanced(on);
    };
    // What the hidden settings are set to, so none is hidden without a
    // word; read again after each save.
    let settings = use_resource(move || async move {
        let _ = saved();
        load_settings().await
    });
    let hidden_set = match &*settings.read() {
        Some(Ok(s)) if !advanced_on() => {
            // A provider's file or scopes are advanced too, though
            // `auth.oidc` is one setting.
            let sso = match tab.as_str() {
                "sso" => crate::providers::advanced_set(value(s, "auth.oidc")),
                _ => 0,
            };
            advanced::set_on_tab(s, &tab) + sso
        }
        _ => 0,
    };
    rsx! {
        div { class: "flex flex-col gap-[22px]",
            div { class: "page-head",
                h1 { class: "page-title page-title-admin", "Settings" }
                label { class: "flex items-center gap-2.5 text-sm text-ctp-subtext0",
                    button {
                        class: "switch",
                        role: "switch",
                        aria_checked: "{advanced_on()}",
                        aria_label: "Show advanced settings",
                        onclick: move |_| set_advanced(!advanced_on()),
                    }
                    "Show advanced settings"
                }
            }
            // Read again after each save, which may be what it counts.
            for n in [saved()] {
                PendingRestart { key: "{n}" }
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
            if hidden_set > 0 {
                span { class: "note -mt-2.5",
                    "{advanced::set_line(hidden_set)} · "
                    button { onclick: move |_| set_advanced(true), "Show" }
                }
            }
            // Telemetry is two cards, Prometheus and OpenTelemetry, as its
            // board draws it.
            if tab == "telemetry" {
                div { class: "flex max-w-[800px] flex-col gap-4",
                    for n in [saved()] {
                        TelemetryForm { key: "{n}", on_done }
                    }
                }
            } else if tab == "remote" {
                // The live status card, then the form, as the
                // WebRemoteAccess board draws them (P6-02).
                div { class: "flex max-w-[820px] flex-col gap-[18px]",
                    for n in [saved()] {
                        RemoteAccessForm { key: "{n}", on_done }
                    }
                }
            } else if tab == "sso" {
                div { class: "flex max-w-[800px] flex-col gap-4",
                    for n in [saved()] {
                        SsoForm { key: "{n}", on_done }
                    }
                }
            } else if tab == "security" {
                // Cookies, then the media tools (PX-22), a card each: each
                // saves on its own.
                div { class: "flex max-w-[800px] flex-col gap-4",
                    section { class: "card",
                        div { class: "card-body form gap-[18px]",
                            for n in [saved()] {
                                SecurityForm { key: "{n}" }
                            }
                        }
                    }
                    if advanced::shown(advanced_on(), "media.sandbox") {
                        section { class: "card",
                            div { class: "card-body form gap-[18px]",
                                for n in [saved()] {
                                    MediaToolsForm { key: "{n}", on_done }
                                }
                            }
                        }
                    }
                }
            } else {
            section { class: "card max-w-[800px]",
                div { class: "card-body form gap-[18px]",
                    for n in [saved()] {
                        match tab.as_str() {
                            "server" => rsx! { ServerForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "plex" => rsx! { PlexForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "paths" => rsx! { PathsForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "playback" => rsx! { PlaybackForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "access" => rsx! { AccessForm { key: "{n}", mode: Mode::Settings, on_done } },
                            "experimental" => rsx! { ExperimentalForm { key: "{n}", on_done } },
                            _ => rsx! { p { key: "{n}", "There is no such tab." } },
                        }
                    }
                }
            }
            }
        }
        Toast { said: toast }
    }
}
