//! Restarting the server from the web UI: what is waiting for a restart,
//! and what the page says about it.

use spoolframe_admin_api::{RestartMode, Settings};

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
