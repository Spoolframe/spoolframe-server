//! What a setting's row says beside its field: "Changed", "default 20
//! GiB" and "Reset to default", and the counts an area shows ("3 changed
//! · 1 restart"). The defaults come from the server (`SettingEntry`'s
//! `default`), never typed into a page.

use serde_json::Value;
use spoolframe_admin_api::{SettingArea, SettingEntry, SettingIndexEntry, SettingLayer};

/// Whether `e` is set to other than its built-in default, by the config
/// file or here. A secret says only whether it is set, so it is never
/// "changed".
pub fn is_changed(e: &SettingEntry) -> bool {
    !e.secret
        && matches!(e.layer, Some(SettingLayer::File | SettingLayer::Runtime))
        && e.value != e.default
}

/// Whether "Reset to default" is offered: it was changed here, and
/// unsetting it brings the default back. A value the config file sets,
/// locked or overridden here, is the file's to keep ("Use the file's
/// value" is the way back to it).
pub fn can_reset(e: &SettingEntry) -> bool {
    is_changed(e) && e.layer == Some(SettingLayer::Runtime) && e.overrides.is_none() && !e.locked
}

/// `value`, setting `key`'s, as a person reads it: with its unit, and
/// MiB in GiB when it is whole GiB ("20 GiB"). The unit is told by the
/// key's last part (`_mb`, `_secs`, `_percent`, `_mbps`, `_days`).
pub fn shown(key: &str, value: &Value) -> String {
    let last = key.rsplit('.').next().unwrap_or(key);
    match value {
        Value::Bool(true) => "on".into(),
        Value::Bool(false) => "off".into(),
        Value::Null => "none".into(),
        Value::String(s) if s.is_empty() => "none".into(),
        Value::String(s) => s.clone(),
        Value::Array(items) if items.is_empty() => "none".into(),
        Value::Array(items) => items
            .iter()
            .map(|v| shown("", v))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Number(n) => {
            let Some(n) = n.as_u64() else {
                return n.to_string();
            };
            if last.ends_with("_mb") {
                if n >= 1024 && n % 1024 == 0 {
                    format!("{} GiB", n / 1024)
                } else {
                    format!("{n} MiB")
                }
            } else if last.ends_with("_secs") {
                format!("{n} s")
            } else if last.ends_with("_percent") {
                format!("{n} %")
            } else if last.ends_with("_mbps") {
                format!("{n} Mbit/s")
            } else if last.ends_with("_days") {
                format!("{n} days")
            } else {
                n.to_string()
            }
        }
        Value::Object(_) => value.to_string(),
    }
}

/// "default 20 GiB" beside a changed value; `None` when the server gives
/// no default (an older one, or a setting the server does without).
pub fn default_line(e: &SettingEntry) -> Option<String> {
    e.default
        .as_ref()
        .map(|d| format!("default {}", shown(&e.key, d)))
}

/// What the Reset dialog says will happen to `e`, called `label`:
/// "40 GiB goes back to the default, 20 GiB." and when it applies.
pub fn reset_text(e: &SettingEntry) -> String {
    let now = e
        .value
        .as_ref()
        .map_or_else(|| "It".to_owned(), |v| shown(&e.key, v));
    let back = match &e.default {
        Some(d) => format!("{now} goes back to the default, {}.", shown(&e.key, d)),
        None => format!("{now} goes back to the server's own choice."),
    };
    let when = match e.apply {
        spoolframe_admin_api::SettingApply::Live => "It applies at once.",
        spoolframe_admin_api::SettingApply::Restart => "It applies after a restart.",
    };
    format!("{back} {when}")
}

/// What the Reset dialog says on a page with a save bar: the value goes
/// back with the page's other changes, at Save, and then when it applies.
pub fn reset_draft_text(e: &SettingEntry) -> String {
    let now = e
        .value
        .as_ref()
        .map_or_else(|| "It".to_owned(), |v| shown(&e.key, v));
    let back = match &e.default {
        Some(d) => format!("{now} goes back to the default, {}", shown(&e.key, d)),
        None => format!("{now} goes back to the server's own choice"),
    };
    let when = match e.apply {
        spoolframe_admin_api::SettingApply::Live => "it applies at once",
        spoolframe_admin_api::SettingApply::Restart => "it applies after a restart",
    };
    format!("{back} when you save; {when}.")
}

/// The settings areas, in the order the settings pages list them, with
/// their names. Federation and Libraries are pages of their own.
pub const AREAS: &[(SettingArea, &str)] = &[
    (SettingArea::General, "General"),
    (SettingArea::Network, "Network"),
    (SettingArea::SignIn, "Sign-in & apps"),
    (SettingArea::Plex, "Plex"),
    (SettingArea::Transcoding, "Transcoding"),
    (SettingArea::Media, "Media & storage"),
    (SettingArea::Subtitles, "Subtitles"),
    (SettingArea::Monitoring, "Monitoring"),
    (SettingArea::Experimental, "Experimental"),
    (SettingArea::Federation, "Federation"),
    (SettingArea::Libraries, "Libraries"),
];

/// How many settings changed, and how many wait for a restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub changed: usize,
    pub restart: usize,
}

impl Counts {
    /// `entries`' counts.
    pub fn of<'e>(entries: impl IntoIterator<Item = &'e SettingEntry>) -> Self {
        entries.into_iter().fold(Self::default(), |c, e| Counts {
            changed: c.changed + usize::from(is_changed(e)),
            restart: c.restart + usize::from(e.restart_pending),
        })
    }

    /// The counts of the settings `index` places in `area`.
    pub fn in_area(
        entries: &[SettingEntry],
        index: &[SettingIndexEntry],
        area: SettingArea,
    ) -> Self {
        Self::of(
            entries
                .iter()
                .filter(|e| index.iter().any(|i| i.key == e.key && i.area == area)),
        )
    }

    /// "3 changed · 1 restart", as a sub-nav shows it; `None` when neither.
    pub fn line(self) -> Option<String> {
        match (self.changed, self.restart) {
            (0, 0) => None,
            (c, 0) => Some(format!("{c} changed")),
            (0, r) => Some(format!("{r} restart")),
            (c, r) => Some(format!("{c} changed · {r} restart")),
        }
    }

    /// "4 changed · 1 waits for restart", as a page's strip says it;
    /// `None` when neither.
    pub fn strip(self) -> Option<String> {
        let waits = match self.restart {
            1 => "1 waits for restart".to_owned(),
            n => format!("{n} wait for restart"),
        };
        match (self.changed, self.restart) {
            (0, 0) => None,
            (c, 0) => Some(format!("{c} changed")),
            (0, _) => Some(waits),
            (c, _) => Some(format!("{c} changed · {waits}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spoolframe_admin_api::SettingApply;

    use super::*;

    fn entry(key: &str, value: Option<Value>, default: Option<Value>) -> SettingEntry {
        SettingEntry {
            key: key.into(),
            set: value.is_some(),
            value,
            secret: false,
            layer: Some(SettingLayer::Runtime),
            locked: false,
            overrides: None,
            apply: SettingApply::Live,
            restart_pending: false,
            running: None,
            default,
        }
    }

    #[test]
    fn a_value_set_here_to_other_than_the_default_is_changed_and_resets() {
        let e = entry(
            "transcode.scratch_limit_mb",
            Some(json!(40960)),
            Some(json!(20480)),
        );
        assert!(is_changed(&e));
        assert!(can_reset(&e));
        assert_eq!(default_line(&e).as_deref(), Some("default 20 GiB"));
    }

    #[test]
    fn a_value_set_to_the_default_itself_is_not_changed() {
        let e = entry("transcode.max_jobs", Some(json!(2)), Some(json!(2)));
        assert!(!is_changed(&e));
        assert!(!can_reset(&e));
    }

    #[test]
    fn a_default_left_alone_is_not_changed() {
        let mut e = entry("transcode.max_jobs", Some(json!(2)), Some(json!(2)));
        e.layer = Some(SettingLayer::Default);
        assert!(!is_changed(&e));
    }

    #[test]
    fn the_config_file_s_value_is_changed_but_has_no_reset() {
        let mut e = entry("transcode.max_jobs", Some(json!(4)), Some(json!(2)));
        e.layer = Some(SettingLayer::File);
        e.locked = true;
        assert!(is_changed(&e));
        assert!(!can_reset(&e));
    }

    #[test]
    fn a_value_overriding_the_file_goes_back_to_the_file_not_the_default() {
        let mut e = entry(
            "transcode.read_ahead_secs",
            Some(json!(90)),
            Some(json!(60)),
        );
        e.overrides = Some(json!(60));
        assert!(is_changed(&e));
        assert!(!can_reset(&e));
    }

    #[test]
    fn a_secret_is_never_changed() {
        let mut e = entry("plex.token", None, None);
        e.secret = true;
        e.set = true;
        assert!(!is_changed(&e));
    }

    #[test]
    fn a_setting_with_no_default_set_here_is_changed_and_says_no_default() {
        let e = entry("plex.url", Some(json!("http://tower:32400")), None);
        assert!(is_changed(&e));
        assert_eq!(default_line(&e), None);
        assert_eq!(
            reset_text(&e),
            "http://tower:32400 goes back to the server's own choice. It applies at once."
        );
    }

    #[test]
    fn values_read_with_their_units() {
        assert_eq!(shown("previews.cache_mb", &json!(4096)), "4 GiB");
        assert_eq!(shown("previews.memory_mb", &json!(64)), "64 MiB");
        assert_eq!(shown("transcode.read_ahead_secs", &json!(60)), "60 s");
        assert_eq!(shown("watch.watched_percent", &json!(95)), "95 %");
        assert_eq!(shown("network.uplink.budget_mbps", &json!(0)), "0 Mbit/s");
        assert_eq!(shown("auth.trust_idle_days", &json!(180)), "180 days");
        assert_eq!(shown("network.mdns", &json!(true)), "on");
        assert_eq!(shown("network.relays", &json!([])), "none");
        assert_eq!(shown("transcode.max_jobs", &json!(2)), "2");
    }

    #[test]
    fn the_reset_dialog_says_what_comes_back_and_when() {
        let mut e = entry(
            "transcode.scratch_limit_mb",
            Some(json!(40960)),
            Some(json!(20480)),
        );
        assert_eq!(
            reset_text(&e),
            "40 GiB goes back to the default, 20 GiB. It applies at once."
        );
        e.apply = SettingApply::Restart;
        assert!(reset_text(&e).ends_with("It applies after a restart."));
    }

    #[test]
    fn counts_say_what_changed_and_what_waits() {
        let mut waiting = entry("transcode.gpus", Some(json!([1])), Some(json!([])));
        waiting.restart_pending = true;
        let entries = [
            entry("transcode.max_jobs", Some(json!(4)), Some(json!(2))),
            entry("transcode.max_remux", Some(json!(32)), Some(json!(32))),
            waiting,
        ];
        let counts = Counts::of(&entries);
        assert_eq!(
            counts,
            Counts {
                changed: 2,
                restart: 1
            }
        );
        assert_eq!(counts.line().as_deref(), Some("2 changed · 1 restart"));
        assert_eq!(
            counts.strip().as_deref(),
            Some("2 changed · 1 waits for restart")
        );
        assert_eq!(Counts::default().line(), None);
        assert_eq!(
            Counts {
                changed: 0,
                restart: 2
            }
            .strip()
            .as_deref(),
            Some("2 wait for restart")
        );
    }

    #[test]
    fn a_reset_on_a_page_with_a_save_bar_waits_for_save() {
        let mut e = entry(
            "transcode.scratch_limit_mb",
            Some(json!(40960)),
            Some(json!(20480)),
        );
        e.apply = SettingApply::Restart;
        assert_eq!(
            reset_draft_text(&e),
            "40 GiB goes back to the default, 20 GiB when you save; it applies after a restart."
        );
    }
}
