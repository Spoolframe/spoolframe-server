//! Server › Settings' "Show advanced settings" (PX-22): the settings few
//! servers need, hidden until it is on. Which they are, and where, is
//! here and nowhere else; the switch itself is kept per browser.

use spoolframe_admin_api::{SettingLayer, Settings};

/// The advanced settings, by the Settings tab each is on. Everything the
/// web UI set before PX-22 stays out of this list: hiding it would move
/// what people already know where to find.
const BY_TAB: &[(&str, &[&str])] = &[
    ("server", &["network.port", "network.plex_discovery_port"]),
    (
        "remote",
        &[
            "network.relays",
            "network.own_relays",
            "network.pkarr",
            "network.congestion_control",
            "network.announce.add",
            "network.announce.ignore",
            "network.announce.ignore_interfaces",
            "network.announce.interfaces",
        ],
    ),
    (
        "playback",
        &[
            "previews.interval_secs",
            "previews.cache_mb",
            "previews.memory_mb",
            "artwork.cache_mb",
            "artwork.memory_mb",
            "catalog.memory_mb",
            "watch.started_after_secs",
            "watch.watched_percent",
            "transcode.gpus",
        ],
    ),
    (
        "security",
        &[
            "media.sandbox",
            "media.ffmpeg",
            "media.ffprobe",
            "media.tesseract",
        ],
    ),
];

/// Where the switch is kept in this browser.
pub const STORAGE_KEY: &str = "spoolframe.settings.advanced";

/// Whether `key` is shown only with "Show advanced settings" on.
pub fn is_advanced(key: &str) -> bool {
    BY_TAB.iter().any(|(_, keys)| keys.contains(&key))
}

/// Whether setting `key`'s field is shown, with the switch `on` or off.
pub fn shown(on: bool, key: &str) -> bool {
    on || !is_advanced(key)
}

/// The advanced settings on Settings tab `tab`.
pub fn on_tab(tab: &str) -> &'static [&'static str] {
    BY_TAB
        .iter()
        .find(|(t, _)| *t == tab)
        .map_or(&[], |(_, keys)| keys)
}

/// How many of `tab`'s advanced settings are set to other than their
/// default, or wait for a restart: what the tab says while they are
/// hidden, so none is hidden without a word.
pub fn set_on_tab(settings: &Settings, tab: &str) -> usize {
    let keys = on_tab(tab);
    settings
        .entries
        .iter()
        .filter(|e| keys.contains(&e.key.as_str()))
        .filter(|e| {
            matches!(e.layer, Some(SettingLayer::File | SettingLayer::Runtime)) || e.restart_pending
        })
        .count()
}

/// The quiet line on a tab with `n` advanced settings set and hidden.
pub fn set_line(n: usize) -> String {
    match n {
        1 => "1 advanced setting is set here".into(),
        n => format!("{n} advanced settings are set here"),
    }
}

/// The switch as kept: on only when it says so.
pub fn kept(value: Option<&str>) -> bool {
    value == Some("on")
}

/// The switch as it is kept.
pub fn keep(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_admin_api::{RestartMode, SettingApply, SettingEntry};

    fn entry(key: &str, layer: Option<SettingLayer>, restart_pending: bool) -> SettingEntry {
        SettingEntry {
            key: key.into(),
            value: None,
            secret: false,
            set: layer.is_some(),
            layer,
            locked: false,
            overrides: None,
            apply: SettingApply::Restart,
            restart_pending,
            running: None,
            deprecated: None,
        }
    }

    fn settings(entries: Vec<SettingEntry>) -> Settings {
        Settings {
            entries,
            config_file: None,
            can_restart: true,
            restart_mode: RestartMode::InPlace,
        }
    }

    #[test]
    fn the_niche_settings_are_advanced() {
        for key in [
            "network.port",
            "network.relays",
            "network.own_relays",
            "network.pkarr",
            "network.congestion_control",
            "network.plex_discovery_port",
            "network.announce.add",
            "network.announce.ignore",
            "network.announce.ignore_interfaces",
            "network.announce.interfaces",
            "media.sandbox",
            "media.ffmpeg",
            "media.ffprobe",
            "media.tesseract",
            "previews.interval_secs",
            "previews.cache_mb",
            "previews.memory_mb",
            "artwork.cache_mb",
            "artwork.memory_mb",
            "catalog.memory_mb",
            "watch.started_after_secs",
            "watch.watched_percent",
            "transcode.gpus",
        ] {
            assert!(is_advanced(key), "{key}");
        }
    }

    #[test]
    fn what_the_ui_set_before_and_previews_when_are_not() {
        for key in [
            "name",
            "network.wan",
            "network.ipv6",
            "admin.public_url",
            "previews.when",
            "previews.workers",
            "auth.oidc",
        ] {
            assert!(!is_advanced(key), "{key}");
        }
    }

    /// Settings › Experimental is shown whole, the switch on or off: AirPlay
    /// is looked for, and the tab is where experimental switches are.
    #[test]
    fn the_experimental_tab_s_settings_are_not() {
        for key in [
            "http_playback.listen",
            "http_playback.address",
            "experimental.libplacebo_shared_on_amd",
        ] {
            assert!(!is_advanced(key), "{key}");
        }
        assert!(on_tab("experimental").is_empty());
    }

    #[test]
    fn an_advanced_field_shows_only_with_the_switch_on() {
        assert!(!shown(false, "network.port"));
        assert!(shown(true, "network.port"));
        assert!(shown(false, "previews.when"));
    }

    #[test]
    fn a_tab_counts_its_advanced_settings_set_or_waiting_for_a_restart() {
        let listed = settings(vec![
            entry("network.relays", Some(SettingLayer::Runtime), false),
            entry("network.pkarr", Some(SettingLayer::Default), false),
            entry("network.own_relays", Some(SettingLayer::File), false),
            entry("network.announce.add", Some(SettingLayer::Default), true),
            entry("network.announce.ignore", None, false),
            // Advanced, but on another tab.
            entry("network.port", Some(SettingLayer::Runtime), false),
            // Set, but not advanced.
            entry("network.wan", Some(SettingLayer::Runtime), false),
        ]);
        assert_eq!(set_on_tab(&listed, "remote"), 3);
        assert_eq!(set_on_tab(&listed, "telemetry"), 0);
    }

    #[test]
    fn the_line_counts_in_the_singular_and_plural() {
        assert_eq!(set_line(1), "1 advanced setting is set here");
        assert_eq!(set_line(2), "2 advanced settings are set here");
    }

    #[test]
    fn the_switch_is_off_unless_kept_on() {
        assert!(kept(Some(keep(true))));
        assert!(!kept(Some(keep(false))));
        assert!(!kept(None));
        assert!(!kept(Some("garbage")));
    }
}
