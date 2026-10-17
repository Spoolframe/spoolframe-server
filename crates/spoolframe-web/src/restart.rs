//! Restarting the server from the web UI: what is waiting for a restart,
//! and what the page says about it.

use serde_json::Value;
use spoolframe_admin_api::{RestartMode, SettingEntry, Settings};

/// The settings changed since the server started that apply only at the
/// next start, by key, in the order the server lists them.
pub fn pending(settings: &Settings) -> Vec<String> {
    settings
        .entries
        .iter()
        .filter(|e| e.restart_pending)
        .map(|e| e.key.clone())
        .collect()
}

/// The banner's sentence for `count` pending changes.
pub fn pending_text(count: usize) -> String {
    match count {
        1 => "1 change takes effect after a restart".into(),
        n => format!("{n} changes take effect after a restart"),
    }
}

/// What the confirm dialog says a restart does, the way `mode` brings the
/// server back.
pub fn what_happens(mode: RestartMode) -> &'static str {
    match mode {
        RestartMode::InPlace => {
            "The server stops and starts itself again. Connected devices drop for a few seconds."
        }
        RestartMode::Supervisor => {
            "The server stops, and its service manager starts it again. Connected devices drop for a few seconds."
        }
    }
}

/// A setting's value as the "Now" and "After restart" lines say it: text
/// as it is, a list's entries joined, and nothing as "not set".
pub fn said(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "not set".into(),
        Some(Value::String(s)) if s.is_empty() => "not set".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) if items.is_empty() => "none".into(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| said(Some(v)))
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => other.to_string(),
    }
}

/// What the server runs with now and what it will after a restart (PX-22),
/// for a restart setting changed since it started; `None` otherwise. An
/// address or a port is the difference between reaching it and not.
pub fn now_and_after(entry: &SettingEntry) -> Option<(String, String)> {
    entry
        .restart_pending
        .then(|| (said(entry.running.as_ref()), said(entry.value.as_ref())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_admin_api::{SettingApply, SettingEntry};

    fn entry(key: &str, apply: SettingApply, restart_pending: bool) -> SettingEntry {
        SettingEntry {
            key: key.into(),
            value: None,
            secret: false,
            set: true,
            layer: None,
            locked: false,
            overrides: None,
            apply,
            restart_pending,
            running: None,
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
    fn only_changed_restart_settings_are_pending() {
        let listed = settings(vec![
            entry("name", SettingApply::Live, false),
            entry("network.port", SettingApply::Restart, true),
            entry("network.mdns", SettingApply::Restart, false),
            entry("admin.listen", SettingApply::Restart, true),
        ]);
        assert_eq!(pending(&listed), ["network.port", "admin.listen"]);
    }

    #[test]
    fn a_pending_port_says_now_and_after() {
        let mut e = entry("network.port", SettingApply::Restart, true);
        e.running = Some(serde_json::json!(7420));
        e.value = Some(serde_json::json!(7440));
        assert_eq!(now_and_after(&e), Some(("7420".into(), "7440".into())));
    }

    #[test]
    fn nothing_set_at_start_is_said_as_not_set() {
        let mut e = entry("network.port", SettingApply::Restart, true);
        e.value = Some(serde_json::json!(7440));
        assert_eq!(now_and_after(&e), Some(("not set".into(), "7440".into())));
    }

    #[test]
    fn a_list_is_said_joined_and_an_empty_one_as_none() {
        let mut e = entry("network.relays", SettingApply::Restart, true);
        e.running = Some(serde_json::json!(["default", "https://relay.example.org"]));
        e.value = Some(serde_json::json!([]));
        assert_eq!(
            now_and_after(&e),
            Some(("default, https://relay.example.org".into(), "none".into()))
        );
    }

    #[test]
    fn a_setting_not_waiting_for_a_restart_says_nothing() {
        let e = entry("admin.listen", SettingApply::Restart, false);
        assert_eq!(now_and_after(&e), None);
    }

    #[test]
    fn one_change_is_said_in_the_singular() {
        assert_eq!(pending_text(1), "1 change takes effect after a restart");
    }

    #[test]
    fn several_changes_are_counted() {
        assert_eq!(pending_text(3), "3 changes take effect after a restart");
    }

    #[test]
    fn a_restart_in_place_says_the_server_starts_itself_again() {
        assert_eq!(
            what_happens(RestartMode::InPlace),
            "The server stops and starts itself again. Connected devices drop for a few seconds."
        );
    }

    #[test]
    fn a_supervised_restart_says_the_supervisor_starts_it_again() {
        assert_eq!(
            what_happens(RestartMode::Supervisor),
            "The server stops, and its service manager starts it again. Connected devices drop for a few seconds."
        );
    }
}
