//! How a settings page's fields read and write their settings (the
//! "Settings, reorganised" boards). Where each setting is, and its
//! widget, unit, bounds and options, is the server's `settingsSchema`;
//! this is what the web does with it: a box's text as a value, a page's
//! edits collected in a [`Draft`] until Save, and the lines around it.

use std::collections::BTreeMap;

use serde_json::Value;
use spoolframe_admin_api::{
    CustomField, FieldWidget, SchemaField, SchemaPage, SettingApply, SettingBounds, SettingEntry,
    SettingLayer,
};

use crate::setup;

/// Where an old Settings tab's address (`/admin/settings/remote`) now
/// leads: its area's slug. `None` for a name that was no tab.
pub fn moved_tab(tab: &str) -> Option<&'static str> {
    Some(match tab {
        "server" => "general",
        "access" | "remote" => "network",
        "security" | "sso" | "mcp" => "sign-in",
        "playback" => "transcoding",
        "telemetry" => "monitoring",
        "paths" => "libraries",
        _ => return None,
    })
}

/// One edit not yet saved.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    Set(Value),
    /// The runtime value out: the file's or the default applies.
    Unset,
}

/// A page's edits until Save, and the fields whose text does not read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub edits: BTreeMap<String, Edit>,
    pub errors: BTreeMap<String, String>,
}

impl Draft {
    /// Field `key`, saved as `saved`, now wants `wanted`: `Ok(None)` for
    /// its default (an empty box), `Err` for text that does not read. An
    /// edit back to what is saved is no edit.
    pub fn put(
        &mut self,
        key: &str,
        wanted: Result<Option<Value>, String>,
        saved: Option<&SettingEntry>,
    ) {
        self.errors.remove(key);
        self.edits.remove(key);
        let runtime = saved.is_some_and(|e| e.layer == Some(SettingLayer::Runtime));
        match wanted {
            Err(why) => {
                self.errors.insert(key.to_owned(), why);
            }
            Ok(None) if runtime => {
                self.edits.insert(key.to_owned(), Edit::Unset);
            }
            Ok(None) => {}
            Ok(Some(value)) => {
                if saved.and_then(|e| e.value.as_ref()) != Some(&value) {
                    self.edits.insert(key.to_owned(), Edit::Set(value));
                }
            }
        }
    }

    /// What field `key` shows: its edit, else what is saved.
    pub fn value<'a>(&'a self, key: &str, saved: Option<&'a SettingEntry>) -> Option<&'a Value> {
        match self.edits.get(key) {
            Some(Edit::Set(v)) => Some(v),
            Some(Edit::Unset) => saved.and_then(|e| e.default.as_ref()),
            None => saved.and_then(|e| e.value.as_ref()),
        }
    }

    /// How many fields are edited, readable or not.
    pub fn count(&self) -> usize {
        self.edits.len()
            + self
                .errors
                .keys()
                .filter(|k| !self.edits.contains_key(*k))
                .count()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// How many of the edits apply after a restart.
    pub fn restarts(&self, entries: &[SettingEntry]) -> usize {
        self.edits
            .keys()
            .filter(|k| {
                entries
                    .iter()
                    .any(|e| &e.key == *k && e.apply == SettingApply::Restart)
            })
            .count()
    }

    /// The edits in the order Save makes them: `page`'s Sensitive ones
    /// first, so a "Confirm it's you" comes before anything is saved.
    pub fn in_order(&self, page: &SchemaPage) -> Vec<(String, Edit)> {
        let mut all: Vec<_> = self.edits.clone().into_iter().collect();
        all.sort_by_key(|(k, _)| risk(page, k).is_none());
        all
    }
}

/// What saving `key` on `page` does, if it is in the Sensitive block.
fn risk<'a>(page: &'a SchemaPage, key: &str) -> Option<&'a str> {
    page.sections
        .iter()
        .flat_map(|s| &s.fields)
        .find(|f| f.key == key && f.risk.is_some())
        .and_then(|f| f.risk.as_deref())
}

/// What "Confirm it's you" says the save does: the first Sensitive edit's
/// consequence, or plain saving.
pub fn confirm_about(draft: &Draft, page: &SchemaPage) -> String {
    draft
        .edits
        .keys()
        .find_map(|k| risk(page, k))
        .map_or_else(|| "saving these settings".to_owned(), str::to_owned)
}

/// The footer's line: "3 unsaved changes".
/// Whether going from the page at `here` to `to` (each a route's
/// address) asks "Leave with N unsaved changes?" first: only with edits
/// unsaved, and only for another page. A jump to a field of the same
/// area (search and commands) keeps the draft, so it goes at once.
pub fn asks_to_leave(unsaved: usize, here: &str, to: &str) -> bool {
    unsaved > 0 && here != to
}

pub fn unsaved_line(n: usize) -> String {
    match n {
        1 => "1 unsaved change".into(),
        n => format!("{n} unsaved changes"),
    }
}

/// The footer's restart note: "1 applies after a restart".
pub fn restart_note(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("1 applies after a restart".into()),
        n => Some(format!("{n} apply after a restart")),
    }
}

/// After a save: "1 change waits for a restart: GPUs." with the labels
/// of the saved settings that wait.
pub fn saved_line(waiting: &[String]) -> String {
    match waiting.len() {
        0 => "Saved.".into(),
        1 => format!("Saved. 1 change waits for a restart: {}.", waiting[0]),
        n => format!(
            "Saved. {n} changes wait for a restart: {}.",
            waiting.join(", ")
        ),
    }
}

/// What a box shows of `value`, as `field` reads it.
pub fn text_of(field: &SchemaField, value: Option<&Value>) -> String {
    match (field.widget, value) {
        (_, None | Some(Value::Null)) => String::new(),
        (FieldWidget::Gib, Some(Value::Number(n))) => {
            n.as_i64().map(setup::gib).unwrap_or_default()
        }
        (_, Some(Value::String(s))) => s.clone(),
        (_, Some(Value::Number(n))) => n.to_string(),
        (_, Some(other)) => other.to_string(),
    }
}

/// What a box's `text` sets `field` to: empty is the default (`None`). A
/// number outside the field's bounds says what it may be, before it is
/// sent.
pub fn read(field: &SchemaField, text: &str) -> Result<Option<Value>, String> {
    let label = &field.label;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let number =
        matches!(field.widget, FieldWidget::Number) || field.custom == Some(CustomField::Budget);
    if number {
        let n = text
            .parse::<i64>()
            .map_err(|_| format!("{label} is a whole number."))?;
        return within(field, n).map(|()| Some(Value::from(n)));
    }
    match field.widget {
        FieldWidget::Gib => {
            let n = setup::mib(text)
                .ok_or_else(|| format!("{label} is a number of GiB, more than 0."))?;
            within(field, n).map(|()| Some(Value::from(n)))
        }
        FieldWidget::EnvRef => {
            let example = field.env_example.as_deref().unwrap_or("SF_ENV_SECRET");
            crate::subtitle_search::secret_reference(label, text, example)
        }
        _ => Ok(Some(Value::String(text.to_owned()))),
    }
}

/// `Ok` if `n` is within `field`'s bounds; else what it may be, in the
/// units it is typed in (GiB for a size kept in MiB).
fn within(field: &SchemaField, n: i64) -> Result<(), String> {
    let Some(SettingBounds { min, max }) = field.bounds else {
        return Ok(());
    };
    if n >= min && max.is_none_or(|max| n <= max) {
        return Ok(());
    }
    let shown = |v: i64| match field.widget {
        // A bound that is no whole GiB (1 MiB) reads as it is kept.
        FieldWidget::Gib if v % 1024 != 0 => format!("{v} MiB"),
        FieldWidget::Gib => format!("{} GiB", setup::gib(v)),
        _ if field.unit.is_empty() => v.to_string(),
        _ => format!("{v} {}", field.unit),
    };
    let label = &field.label;
    Err(match max {
        Some(max) => format!("{label} is from {} to {}.", shown(min), shown(max)),
        None => format!("{label} is at least {}.", shown(min)),
    })
}

thread_local! {
    static KEYS: std::cell::RefCell<std::collections::HashSet<&'static str>> =
        std::cell::RefCell::default();
}

/// A setting's key, as the fields hold it: one string for the page's
/// life. The keys are a fixed set the server names (about 120), each
/// kept once, so a field's key is `Copy` like the rest of its props.
pub fn key(k: &str) -> &'static str {
    KEYS.with(|keys| {
        let mut keys = keys.borrow_mut();
        if let Some(kept) = keys.get(k) {
            return *kept;
        }
        let kept: &'static str = Box::leak(k.to_owned().into_boxed_str());
        keys.insert(kept);
        kept
    })
}

/// A list's items, from its value.
pub fn items(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A list's value from its items; blank ones are left out.
pub fn list_value(items: &[String]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_owned()))
            .collect(),
    )
}

/// What the Overview's card for `page` says now, from the settings.
pub fn summary(slug: &str, entries: &[SettingEntry]) -> String {
    let value = |key: &str| {
        entries
            .iter()
            .find(|e| e.key == key)
            .and_then(|e| e.value.as_ref())
    };
    let on = |key: &str| value(key) == Some(&Value::Bool(true));
    let text = |key: &str| {
        value(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match slug {
        "general" => {
            let found = if on("network.mdns") {
                "found on this network"
            } else {
                "not announced on this network"
            };
            format!("{} · {found}", text("name"))
        }
        "network" => {
            let remote = if value("network.wan").is_some_and(|v| v.is_object()) {
                "Remote on"
            } else {
                "Remote off"
            };
            match text("admin.public_url").as_str() {
                "" => remote.to_owned(),
                url => format!("{remote} · {url}"),
            }
        }
        "sign-in" => {
            let providers = items(value("auth.oidc")).len();
            let mcp = if on("mcp.enabled") {
                "MCP on"
            } else {
                "MCP off"
            };
            match providers {
                1 => format!("1 provider · {mcp}"),
                n => format!("{n} providers · {mcp}"),
            }
        }
        "plex" => match text("plex.url").as_str() {
            "" => "No Plex server".into(),
            url => url.to_owned(),
        },
        "transcoding" => {
            let jobs = value("transcode.max_jobs")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!("{jobs} encodes at once")
        }
        "media" => match text("previews.when").as_str() {
            "on_play" => "Previews when played".into(),
            "never" => "No previews".into(),
            _ => "Previews made ahead".into(),
        },
        "subtitles" => {
            let ocr = if on("subtitles.ocr") {
                "OCR on"
            } else {
                "OCR off"
            };
            let sites = items(value("subtitles.sources"));
            match sites.len() {
                0 => ocr.to_owned(),
                _ => format!("{} · {ocr}", sites.join(", ")),
            }
        }
        "monitoring" => {
            let metrics = if on("admin.metrics") {
                "/metrics on"
            } else {
                "/metrics off"
            };
            let otlp = if text("telemetry.otlp.endpoint").is_empty() {
                "telemetry off"
            } else {
                "telemetry on"
            };
            format!("{metrics} · {otlp}")
        }
        "experimental" => {
            let flags = ["experimental.index_ahead", "experimental.smart_crop"];
            match flags.iter().filter(|k| on(k)).count() {
                0 => "All off".into(),
                n => format!("{n} on"),
            }
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    /// A link or a search hit to another page asks while edits are
    /// unsaved; a jump to a field of the same area does not, nor does
    /// anything with no edits.
    #[test]
    fn only_leaving_the_page_with_edits_asks() {
        let here = "/admin/settings/network";
        assert!(asks_to_leave(2, here, "/admin/settings/general"));
        assert!(asks_to_leave(
            1,
            "/admin/federation?tab=settings",
            "/admin/federation?tab="
        ));
        assert!(!asks_to_leave(2, here, here));
        assert!(!asks_to_leave(0, here, "/admin/settings/general"));
    }

    use serde_json::json;

    use super::*;

    fn entry(key: &str, value: Value, layer: SettingLayer) -> SettingEntry {
        SettingEntry {
            key: key.into(),
            value: Some(value),
            secret: false,
            set: true,
            layer: Some(layer),
            locked: false,
            overrides: None,
            apply: SettingApply::Restart,
            restart_pending: false,
            running: None,
            default: Some(json!(20480)),
        }
    }

    #[test]
    fn old_tab_addresses_go_to_their_area() {
        assert_eq!(moved_tab("server"), Some("general"));
        assert_eq!(moved_tab("remote"), Some("network"));
        assert_eq!(moved_tab("sso"), Some("sign-in"));
        assert_eq!(moved_tab("playback"), Some("transcoding"));
        assert_eq!(moved_tab("telemetry"), Some("monitoring"));
        assert_eq!(moved_tab("paths"), Some("libraries"));
        // An area's own name is no old tab: the schema has it.
        assert_eq!(moved_tab("network"), None);
        assert_eq!(moved_tab("nonsense"), None);
    }

    #[test]
    fn an_edit_back_to_the_saved_value_is_no_edit() {
        let saved = entry(
            "transcode.scratch_limit_mb",
            json!(40960),
            SettingLayer::Runtime,
        );
        let mut draft = Draft::default();
        draft.put(&saved.key, Ok(Some(json!(20480))), Some(&saved));
        assert_eq!(draft.count(), 1);
        draft.put(&saved.key, Ok(Some(json!(40960))), Some(&saved));
        assert!(draft.is_empty());
    }

    #[test]
    fn an_empty_box_unsets_a_runtime_value_and_nothing_else() {
        let runtime = entry("name", json!("Home"), SettingLayer::Runtime);
        let file = entry("name", json!("Home"), SettingLayer::File);
        let mut draft = Draft::default();
        draft.put("name", Ok(None), Some(&runtime));
        assert_eq!(draft.edits.get("name"), Some(&Edit::Unset));
        // An unset shows the default.
        assert_eq!(draft.value("name", Some(&runtime)), Some(&json!(20480)));
        draft.put("name", Ok(None), Some(&file));
        assert!(draft.is_empty());
    }

    #[test]
    fn text_that_does_not_read_counts_and_says_why() {
        let saved = entry("transcode.max_jobs", json!(2), SettingLayer::Default);
        let mut draft = Draft::default();
        let budget = number("transcode.max_jobs", "", "Encode budget");
        draft.put(&saved.key, read(&budget, "two"), Some(&saved));
        assert_eq!(draft.count(), 1);
        assert_eq!(draft.errors[&saved.key], "Encode budget is a whole number.");
        draft.put(&saved.key, read(&budget, "4"), Some(&saved));
        assert!(draft.errors.is_empty());
        assert_eq!(draft.edits[&saved.key], Edit::Set(json!(4)));
    }

    #[test]
    fn sensitive_edits_are_saved_first_and_named_in_the_confirm() {
        let mut draft = Draft::default();
        draft
            .edits
            .insert("admin.listen".into(), Edit::Set(json!("0.0.0.0:80")));
        draft
            .edits
            .insert("mcp.enabled".into(), Edit::Set(json!(true)));
        let page = SchemaPage {
            slug: "sign-in".into(),
            area: spoolframe_admin_api::SettingArea::SignIn,
            name: "Sign-in & apps".into(),
            line: String::new(),
            place: spoolframe_admin_api::SchemaPlace::Settings,
            sections: vec![spoolframe_admin_api::SchemaSection {
                name: "Sensitive".into(),
                help: String::new(),
                kind: spoolframe_admin_api::SectionKind::Sensitive,
                fields: vec![SchemaField {
                    risk: Some("turning on AI agents (MCP): agents act for users".into()),
                    ..test_field("mcp.enabled", FieldWidget::Switch, "AI agents (MCP)")
                }],
            }],
        };
        let order: Vec<_> = draft.in_order(&page).into_iter().map(|(k, _)| k).collect();
        assert_eq!(order, ["mcp.enabled", "admin.listen"]);
        assert!(confirm_about(&draft, &page).starts_with("turning on AI agents (MCP)"));
        draft.edits.remove("mcp.enabled");
        assert_eq!(confirm_about(&draft, &page), "saving these settings");
    }

    #[test]
    fn the_footer_counts_unsaved_edits_and_restarts() {
        let entries = vec![
            entry("transcode.gpus", json!([]), SettingLayer::Default),
            SettingEntry {
                apply: SettingApply::Live,
                ..entry("transcode.max_jobs", json!(2), SettingLayer::Default)
            },
        ];
        let mut draft = Draft::default();
        draft
            .edits
            .insert("transcode.gpus".into(), Edit::Set(json!(["card0"])));
        draft
            .edits
            .insert("transcode.max_jobs".into(), Edit::Set(json!(4)));
        assert_eq!(unsaved_line(draft.count()), "2 unsaved changes");
        assert_eq!(
            restart_note(draft.restarts(&entries)).unwrap(),
            "1 applies after a restart"
        );
        assert_eq!(restart_note(0), None);
        assert_eq!(
            saved_line(&["GPUs".into()]),
            "Saved. 1 change waits for a restart: GPUs."
        );
        assert_eq!(saved_line(&[]), "Saved.");
    }

    #[test]
    fn gib_boxes_keep_mib() {
        let scratch = test_field(
            "transcode.scratch_limit_mb",
            FieldWidget::Gib,
            "Scratch space",
        );
        assert_eq!(text_of(&scratch, Some(&json!(40960))), "40");
        assert_eq!(read(&scratch, "1.5"), Ok(Some(json!(1536))));
        assert!(read(&scratch, "-1").is_err());
        assert_eq!(read(&scratch, " "), Ok(None));
        let name = test_field("name", FieldWidget::Text, "Name");
        assert_eq!(read(&name, " Home "), Ok(Some(json!("Home"))));
    }

    #[test]
    fn a_secret_kept_as_plain_text_is_named_by_its_variable_only() {
        let field = SchemaField {
            env_example: Some("SF_ENV_SUBDL_KEY".into()),
            ..test_field("subdl.api_key", FieldWidget::EnvRef, "SubDL's API key")
        };
        assert_eq!(
            read(&field, "${SF_ENV_SUBDL_KEY}"),
            Ok(Some(json!("${SF_ENV_SUBDL_KEY}")))
        );
        let pasted = read(&field, "abc123").unwrap_err();
        assert!(pasted.contains("environment variable"), "{pasted}");
    }

    /// A field as the schema sends it, called `label`.
    fn test_field(key: &str, widget: FieldWidget, label: &str) -> SchemaField {
        SchemaField {
            key: key.to_owned(),
            label: label.to_owned(),
            help: String::new(),
            anchor: None,
            apply: SettingApply::Live,
            widget,
            unit: String::new(),
            bounds: None,
            options: Vec::new(),
            add_label: None,
            env_example: None,
            custom: None,
            risk: None,
            recent_sign_in: false,
        }
    }

    fn number(key: &str, unit: &str, label: &str) -> SchemaField {
        SchemaField {
            unit: unit.to_owned(),
            ..test_field(key, FieldWidget::Number, label)
        }
    }

    /// A number outside its bounds says what it may be before it is
    /// sent, in the units it is typed in.
    #[test]
    fn a_number_out_of_bounds_says_what_it_may_be() {
        let share = SchemaField {
            bounds: Some(SettingBounds {
                min: 0,
                max: Some(100),
            }),
            ..number("network.uplink.owner_share_percent", "%", "Admins' share")
        };
        assert_eq!(read(&share, "100"), Ok(Some(json!(100))));
        assert_eq!(
            read(&share, "150"),
            Err("Admins' share is from 0 % to 100 %.".into())
        );
        let workers = SchemaField {
            bounds: Some(SettingBounds { min: 1, max: None }),
            ..number("previews.workers", "", "Made at once")
        };
        assert_eq!(
            read(&workers, "0"),
            Err("Made at once is at least 1.".into())
        );
        let keep = SchemaField {
            bounds: Some(SettingBounds {
                min: 1024,
                max: None,
            }),
            ..test_field("optimized.max_size_mb", FieldWidget::Gib, "Most to keep")
        };
        assert_eq!(
            read(&keep, "0.5"),
            Err("Most to keep is at least 1 GiB.".into())
        );
        let small = SchemaField {
            bounds: Some(SettingBounds { min: 1, max: None }),
            ..keep.clone()
        };
        assert_eq!(
            read(&small, "0.0001"),
            Err("Most to keep is at least 1 MiB.".into())
        );
        // Unbounded, as a field from an older server.
        let free = number("network.port", "", "App port");
        assert_eq!(read(&free, "70000"), Ok(Some(json!(70000))));
    }

    /// The fields hold their key as one string, however often it is
    /// asked for.
    #[test]
    fn a_key_is_kept_once() {
        let a = key(&String::from("network.port"));
        let b = key("network.port");
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn a_list_drops_blank_rows() {
        let items = vec!["10.0.0.0/8".to_owned(), " ".to_owned()];
        assert_eq!(list_value(&items), json!(["10.0.0.0/8"]));
        assert_eq!(super::items(Some(&json!(["a", "b"]))), ["a", "b"]);
    }

    #[test]
    fn the_overview_cards_say_what_is_set() {
        let entries = vec![
            entry("name", json!("Home Media"), SettingLayer::Runtime),
            entry("network.mdns", json!(true), SettingLayer::Default),
            entry("admin.metrics", json!(true), SettingLayer::Runtime),
        ];
        assert_eq!(
            summary("general", &entries),
            "Home Media · found on this network"
        );
        assert_eq!(summary("network", &entries), "Remote off");
        assert_eq!(
            summary("monitoring", &entries),
            "/metrics on · telemetry off"
        );
        assert_eq!(summary("experimental", &entries), "All off");
    }
}
