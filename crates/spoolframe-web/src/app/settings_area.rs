//! One area's settings (the "Settings, reorganised" boards): its
//! sections, each field with its label, help, key and marks, an Advanced
//! section collapsed, the Sensitive block, and the one save model: edits
//! collect in a bar, "N unsaved · Discard · Save", with the restart they
//! need named, and "Confirm it's you" before a sensitive one is saved.
//! Settings' areas, Federation's Settings tab and the Libraries page all
//! draw an area this way.

use std::collections::BTreeSet;

use dioxus::prelude::*;
use serde_json::Value;
use spoolframe_admin_api::{
    AdminApiClient, CustomField, FieldWidget, SchemaField, SchemaPage, SchemaSection, SectionKind,
    SettingApply, SettingEntry, SettingIndexEntry, SettingLayer, SettingOption, Settings,
    SettingsSchema,
};

use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::{Alert, icons, rpc};
use crate::settings_layout::{
    Draft, Edit, confirm_about, items, key as kept_key, list_value, read, restart_note, saved_line,
    text_of, unsaved_line,
};
use crate::settings_marks;

/// An area page's state, for its fields: what is saved, what is edited,
/// and what each setting is called.
#[derive(Clone, Copy)]
pub(super) struct AreaState {
    pub draft: Signal<Draft>,
    pub saved: Signal<Option<Settings>>,
    pub index: Signal<Vec<SettingIndexEntry>>,
    /// Read the settings again, after something saved them at once.
    pub reload: Signal<u32>,
}

impl AreaState {
    pub fn entry(&self, key: &str) -> Option<SettingEntry> {
        self.saved
            .read()
            .as_ref()
            .and_then(|s| s.entries.iter().find(|e| e.key == key).cloned())
    }

    /// What field `key` shows now: its edit, else what is saved.
    pub fn shown(&self, key: &str) -> Option<Value> {
        let entry = self.entry(key);
        self.draft.read().value(key, entry.as_ref()).cloned()
    }

    /// Field `key` wants `wanted` (see [`Draft::put`]).
    pub fn put(&self, key: &str, wanted: Result<Option<Value>, String>) {
        let entry = self
            .saved
            .peek()
            .as_ref()
            .and_then(|s| s.entries.iter().find(|e| e.key == key).cloned());
        let mut draft = self.draft;
        draft.write().put(key, wanted, entry.as_ref());
    }

    /// Forget field `key`'s edit: it shows what is saved again.
    pub fn forget(&self, key: &str) {
        let mut draft = self.draft;
        let mut d = draft.write();
        d.edits.remove(key);
        d.errors.remove(key);
    }

    pub fn label(&self, key: &str) -> String {
        self.index
            .read()
            .iter()
            .find(|i| i.key == key)
            .map_or_else(|| key.to_owned(), |i| i.label.clone())
    }

    pub fn locked(&self, key: &str) -> bool {
        self.entry(key).is_some_and(|e| e.locked)
    }

    /// The config file's path, for "Set by the server's config file".
    pub fn file(&self) -> String {
        self.saved
            .read()
            .as_ref()
            .and_then(|s| s.config_file.clone())
            .unwrap_or_default()
    }
}

/// How many edits the open area holds, for leaving it (the WebSettings
/// States board, 1): the frame asks before a link leaves them behind.
#[derive(Clone, Copy)]
pub(super) struct Unsaved(pub Signal<usize>);

/// Where a link held back by unsaved edits was going, while the frame
/// asks "Leave with N unsaved changes?".
#[derive(Clone, Copy)]
pub(super) struct Leaving(pub Signal<Option<super::Route>>);

/// Whether going to `to` now would leave unsaved edits behind
/// ([`crate::settings_layout::asks_to_leave`]): if so, the frame is told
/// to ask, and `true`; the caller then does not go.
fn held_back(unsaved: Option<Unsaved>, leaving: Option<Leaving>, to: &super::Route) -> bool {
    let n = unsaved.map_or(0, |Unsaved(n)| *n.peek());
    let here = router().current::<super::Route>().to_string();
    let asks = crate::settings_layout::asks_to_leave(n, &here, &to.to_string());
    if asks && let Some(Leaving(mut l)) = leaving {
        l.set(Some(to.clone()));
    }
    asks && leaving.is_some()
}

/// Go to a route from code (search and commands), asking first, as a
/// [`GuardedLink`] does, when it leaves unsaved edits behind.
pub(super) fn use_guarded_push() -> Callback<super::Route> {
    let unsaved = try_use_context::<Unsaved>();
    let leaving = try_use_context::<Leaving>();
    let nav = navigator();
    use_callback(move |to: super::Route| {
        if !held_back(unsaved, leaving, &to) {
            nav.push(to);
        }
    })
}

/// A link that may leave unsaved edits behind: then it asks first
/// instead of going. Any link out of a page with a draft is one.
#[component]
pub(super) fn GuardedLink(
    #[props(default)] class: String,
    to: super::Route,
    #[props(default)] aria_current: Option<String>,
    children: Element,
) -> Element {
    let unsaved = try_use_context::<Unsaved>();
    let leaving = try_use_context::<Leaving>();
    let target = to.clone();
    let here = use_route::<super::Route>().to_string();
    let n = unsaved.map_or(0, |Unsaved(n)| n());
    let held = crate::settings_layout::asks_to_leave(n, &here, &to.to_string());
    rsx! {
        Link {
            class,
            to,
            aria_current,
            onclick_only: held,
            onclick: move |_| {
                if held {
                    held_back(unsaved, leaving, &target);
                }
            },
            {children}
        }
    }
}

/// The settings index, read once a visit.
async fn load_index() -> Vec<SettingIndexEntry> {
    let read = async {
        let client = rpc::client().await?;
        client.settings_index().await.map_err(rpc::error)
    };
    read.await.unwrap_or_default()
}

/// What the save did, under the bar.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Saved(Vec<String>),
    Failed(String),
}

thread_local! {
    static SCHEMA: std::cell::RefCell<Option<std::rc::Rc<SettingsSchema>>> =
        const { std::cell::RefCell::new(None) };
}

/// The server's settings pages (`settingsSchema`): asked once a visit,
/// as they are the same for every server of a version.
pub(super) async fn schema() -> Result<std::rc::Rc<SettingsSchema>, crate::logic::ApiError> {
    if let Some(kept) = SCHEMA.with(|s| s.borrow().clone()) {
        return Ok(kept);
    }
    let client = rpc::client().await?;
    let schema = std::rc::Rc::new(client.settings_schema().await.map_err(rpc::error)?);
    SCHEMA.with(|s| *s.borrow_mut() = Some(schema.clone()));
    Ok(schema)
}

/// Area `slug`'s page, as the server's schema draws it.
#[component]
pub(super) fn AreaPage(slug: String) -> Element {
    let schema = use_resource(schema);
    let page = match &*schema.read() {
        Some(Ok(schema)) => schema.pages.iter().find(|p| p.slug == slug).cloned(),
        Some(Err(e)) => return rsx! { Alert { text: e.explain() } },
        None => return rsx! { p { class: "text-sm text-ctp-subtext0", "Loading…" } },
    };
    match page {
        Some(page) => rsx! { AreaForm { page } },
        None => rsx! { Alert { text: format!("This server has no settings page called {slug}.") } },
    }
}

/// One area's sections, fields and save bar.
#[component]
pub(super) fn AreaForm(page: SchemaPage) -> Element {
    let mut reload = use_signal(|| 0u32);
    let loaded = use_resource(move || async move {
        let _ = reload();
        super::wizard::load_settings().await
    });
    let mut saved = use_signal(|| None::<Settings>);
    let mut index = use_signal(Vec::<SettingIndexEntry>::new);
    let mut draft = use_signal(Draft::default);
    // Discard draws every field anew, from what is saved.
    let mut generation = use_signal(|| 0u32);
    let mut opened = use_signal(BTreeSet::<String>::new);
    let mut busy = use_signal(|| false);
    let mut outcome = use_signal(|| None::<Outcome>);
    let confirming = use_signal(|| None::<String>);
    let state = use_context_provider(|| AreaState {
        draft,
        saved,
        index,
        reload,
    });
    use_future(move || async move {
        index.set(load_index().await);
    });
    use_effect(move || {
        if let Some(Ok(s)) = &*loaded.read() {
            saved.set(Some(s.clone()));
        }
    });
    // The frame asks before leaving edits behind.
    if let Some(Unsaved(mut unsaved)) = try_use_context::<Unsaved>() {
        use_effect(move || unsaved.set(draft.read().count()));
        use_drop(move || unsaved.set(0));
    }
    // A setting search and commands opened: its Advanced section opens
    // first, then the field is shown and marked.
    let jump = try_use_context::<super::palette::SettingJump>();
    let sections = page.sections.clone();
    let saving = page.clone();
    use_effect(move || {
        let Some(super::palette::SettingJump(mut jump)) = jump else {
            return;
        };
        let Some((anchor, _)) = jump() else {
            return;
        };
        let here = sections.iter().find(|s| {
            s.fields
                .iter()
                .any(|f| f.anchor.as_deref() == Some(anchor.as_str()))
        });
        if let Some(section) = here {
            jump.set(None);
            if section.kind == SectionKind::Advanced {
                opened.write().insert(section.name.clone());
            }
            super::palette::show_field(&anchor);
        }
    });

    let refresh = try_use_context::<super::settings::Refresh>();
    let save = use_callback(move |()| {
        let edits = draft.peek().in_order(&saving);
        let about = confirm_about(&draft.peek(), &saving);
        busy.set(true);
        outcome.set(None);
        spawn(async move {
            let client = match rpc::client().await {
                Ok(c) => c,
                Err(e) => {
                    outcome.set(Some(Outcome::Failed(e.explain())));
                    busy.set(false);
                    return;
                }
            };
            let mut waiting = Vec::new();
            for (key, edit) in edits {
                let done = match &edit {
                    Edit::Set(v) => client.setting_set(key.clone(), v.clone()).await,
                    Edit::Unset => client.setting_unset(key.clone()).await,
                };
                match done.map_err(rpc::error) {
                    Ok(entry) => {
                        if entry.apply == SettingApply::Restart && entry.restart_pending {
                            waiting.push(state.label(&key));
                        }
                        draft.write().edits.remove(&key);
                    }
                    // Nothing after it is tried: the rest wait for the
                    // confirm, or for the field to be fixed.
                    Err(e) if asks_to_confirm(&e, confirming, &about) => {
                        busy.set(false);
                        reload += 1;
                        return;
                    }
                    Err(e) => {
                        draft.write().errors.insert(key.clone(), e.explain());
                        outcome.set(Some(Outcome::Failed(format!(
                            "{} was not saved: {}",
                            state.label(&key),
                            e.explain()
                        ))));
                        busy.set(false);
                        reload += 1;
                        return;
                    }
                }
            }
            outcome.set(Some(Outcome::Saved(waiting)));
            busy.set(false);
            reload += 1;
            if let Some(super::settings::Refresh(mut r)) = refresh {
                r += 1;
            }
        });
    });

    if saved.read().is_none() {
        return match &*loaded.read() {
            Some(Err(e)) => rsx! { Alert { text: e.explain() } },
            _ => rsx! { p { class: "text-sm text-ctp-subtext0", "Loading…" } },
        };
    }
    let entries = saved
        .read()
        .as_ref()
        .map(|s| s.entries.clone())
        .unwrap_or_default();
    let count = draft.read().count();
    let fix = draft.read().errors.len();
    let restarts = restart_note(draft.read().restarts(&entries));
    rsx! {
        div { class: "flex flex-col gap-5 pb-24",
            for g in [generation()] {
                div { key: "{g}", class: "flex flex-col gap-5",
                    for section in page.sections.iter().cloned() {
                        SectionView {
                            key: "{section.name}",
                            open: opened.read().contains(&section.name),
                            section,
                            on_toggle: move |name: String| {
                                let mut o = opened.write();
                                if !o.remove(&name) {
                                    o.insert(name);
                                }
                            },
                        }
                    }
                }
            }
            match outcome() {
                Some(Outcome::Saved(waiting)) if !waiting.is_empty() => rsx! {
                    div { class: "saved-strip", role: "status",
                        span { "{saved_line(&waiting)}" }
                        span { class: "flex gap-2",
                            button { class: "btn-dialog btn-dialog-grey", onclick: move |_| outcome.set(None), "Later" }
                            super::restart::RestartButton { label: "Restart now", class: "btn-dialog btn-dialog-primary" }
                        }
                    }
                },
                Some(Outcome::Saved(_)) => rsx! {
                    div { class: "saved-strip", role: "status", span { "Saved." } }
                },
                Some(Outcome::Failed(text)) => rsx! { Alert { text } },
                None => rsx! {},
            }
        }
        if !draft.read().is_empty() {
            div { class: "save-bar", role: "region", aria_label: "Unsaved changes",
                span { class: "flex min-w-0 flex-col",
                    b { "{unsaved_line(count)}" }
                    if fix > 0 {
                        span { class: "text-xs text-ctp-red", "Fix the fields marked in red to save." }
                    } else if let Some(note) = restarts {
                        span { class: "text-xs text-ctp-yellow", "{note}" }
                    }
                }
                span { class: "flex gap-2",
                    button {
                        class: "btn-dialog btn-dialog-grey",
                        disabled: busy(),
                        onclick: move |_| {
                            draft.set(Draft::default());
                            outcome.set(None);
                            generation += 1;
                        },
                        "Discard"
                    }
                    button {
                        class: "btn-dialog btn-dialog-primary",
                        disabled: busy() || fix > 0,
                        onclick: move |_| save.call(()),
                        if busy() { "Saving…" } else { "Save" }
                    }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| save.call(()) }
    }
}

/// How many of `section`'s settings are changed from their defaults.
fn changed_in(section: &SchemaSection, entries: &[SettingEntry]) -> usize {
    section
        .fields
        .iter()
        .filter(|f| {
            entries
                .iter()
                .any(|e| e.key == f.key && settings_marks::is_changed(e))
        })
        .count()
}

#[component]
fn SectionView(section: SchemaSection, open: bool, on_toggle: EventHandler<String>) -> Element {
    let state = use_context::<AreaState>();
    let entries = state
        .saved
        .read()
        .as_ref()
        .map(|s| s.entries.clone())
        .unwrap_or_default();
    let edited = section
        .fields
        .iter()
        .filter(|f| state.draft.read().edits.contains_key(&f.key))
        .count();
    let rows = rsx! {
        for field in section.fields.iter().cloned() {
            FieldRow { key: "{field.key}-{field.widget:?}-{field.custom:?}", field }
        }
    };
    let name = section.name.clone();
    match section.kind {
        SectionKind::Plain | SectionKind::Unknown => rsx! {
            section { class: "card", aria_label: "{section.name}",
                div { class: "card-body gap-[18px]",
                    div { class: "flex flex-col gap-1",
                        h2 { class: "m-0 text-lg font-semibold", "{section.name}" }
                        if !section.help.is_empty() {
                            p { class: "m-0 text-sm text-ctp-subtext0", "{section.help}" }
                        }
                    }
                    {rows}
                }
            }
        },
        SectionKind::Advanced => {
            let changed = changed_in(&section, &entries);
            let more = match (changed, edited) {
                (0, 0) => String::new(),
                (c, 0) => format!(" · {c} changed"),
                (0, e) => format!(" · {e} unsaved"),
                (c, e) => format!(" · {c} changed · {e} unsaved"),
            };
            rsx! {
                section { class: "card", aria_label: "{section.name}",
                    div { class: "card-body gap-[18px]",
                        button {
                            class: "advanced-toggle",
                            r#type: "button",
                            aria_expanded: "{open}",
                            onclick: move |_| on_toggle.call(name.clone()),
                            if open { icons::ChevronUp { size: 16 } } else { icons::ChevronDown { size: 16 } }
                            b { "Advanced" }
                            span { class: "text-ctp-subtext0", " · {section.fields.len()}{more}" }
                            span { class: "ml-auto text-xs text-ctp-subtext0", "{section.help}" }
                        }
                        if open {
                            {rows}
                        }
                    }
                }
            }
        }
        SectionKind::Sensitive => rsx! {
            section { class: "card sensitive", aria_label: "Sensitive",
                div { class: "card-body gap-[18px]",
                    div { class: "flex flex-col gap-1",
                        h2 { class: "m-0 flex items-center gap-2 text-lg font-semibold",
                            icons::ShieldAlert { size: 18 }
                            "Sensitive"
                        }
                        p { class: "m-0 text-sm text-ctp-subtext0", "{section.help}" }
                        p { class: "m-0 text-xs font-semibold text-ctp-peach", "Saving a change here asks you to confirm it's you." }
                    }
                    {rows}
                }
            }
        },
    }
}

/// `entry` as the marks see it with the page's edit in: an edited
/// setting reads as set here, to what it is edited to.
fn as_edited(entry: &SettingEntry, draft: &Draft) -> SettingEntry {
    match draft.edits.get(&entry.key) {
        Some(Edit::Set(v)) => SettingEntry {
            value: Some(v.clone()),
            layer: Some(SettingLayer::Runtime),
            overrides: None,
            ..entry.clone()
        },
        Some(Edit::Unset) => SettingEntry {
            value: entry.default.clone(),
            layer: Some(SettingLayer::Default),
            ..entry.clone()
        },
        None => entry.clone(),
    }
}

/// One setting: what it is called and does, its key, its control, and
/// how it stands: set by the file, changed, waiting for a restart.
#[component]
fn FieldRow(field: SchemaField) -> Element {
    let key = kept_key(&field.key);
    let state = use_context::<AreaState>();
    let mut resetting = use_signal(|| false);
    let Some(entry) = state.entry(key) else {
        // A server without the setting: nothing to show.
        return rsx! {};
    };
    // `network.wan`'s second place, in the Sensitive block, is whether
    // it is there at all.
    let wan_switch = field.custom == Some(CustomField::WanSwitch);
    let (label, help) = if wan_switch {
        (
            "Advertise a public address".to_owned(),
            "On: apps outside your home are told the addresses under Apps from outside.".to_owned(),
        )
    } else {
        (field.label.clone(), field.help.clone())
    };
    let marks = as_edited(&entry, &state.draft.read());
    let error = state.draft.read().errors.get(key).cloned();
    let file = state.file();
    let custom = field.widget == FieldWidget::Custom && !wan_switch;
    let row_class = if error.is_some() {
        "setting-row setting-row-error"
    } else {
        "setting-row"
    };
    // A custom field draws its own label for what it holds, and a switch
    // sits beside its label.
    let control = match field.widget {
        FieldWidget::Switch => rsx! { SwitchBox { setting: key, label: label.clone() } },
        FieldWidget::Choice => rsx! { ChoiceBox { setting: key, options: field.options.clone() } },
        FieldWidget::Many => rsx! { ManyBox { setting: key, options: field.options.clone() } },
        FieldWidget::List => rsx! {
            ListBox {
                setting: key,
                add: field.add_label.clone().unwrap_or_else(|| "Add".to_owned()),
                label: label.clone(),
            }
        },
        FieldWidget::Custom => rsx! {
            super::settings_custom::CustomField { field: field.clone() }
        },
        // A widget a newer server added reads as text here.
        FieldWidget::Text
        | FieldWidget::Secret
        | FieldWidget::EnvRef
        | FieldWidget::Number
        | FieldWidget::Gib
        | FieldWidget::Unknown => rsx! {
            TextBox { field: field.clone() }
        },
    };
    rsx! {
        div {
            class: row_class,
            id: field.anchor.clone(),
            div { class: "setting-text",
                b { "{label}" }
                if !help.is_empty() && !custom {
                    span { class: "text-sm text-ctp-subtext0", "{help}" }
                }
                code { class: "setting-key", "{key}" }
            }
            {control}
            if let Some(why) = error {
                span { class: "text-xs text-ctp-red", role: "alert", "{why}" }
            }
            div { class: "setting-marks",
                if entry.locked {
                    span { class: "note",
                        "Set by the server's config file · "
                        span { class: "mono-value text-xs", "{file}" }
                    }
                } else if let Some(over) = entry.overrides.clone() {
                    if !state.draft.read().edits.contains_key(key) {
                        span { class: "note",
                            "Overrides {settings_marks::shown(key, &over)} from "
                            span { class: "mono-value text-xs", "{file}" }
                            " · "
                            button { class: "link-button", onclick: move |_| state.put(key, Ok(None)), "Use the file's value" }
                        }
                    }
                } else if settings_marks::is_changed(&marks) {
                    span { class: "chip-changed", "Changed" }
                    if let Some(line) = settings_marks::default_line(&marks) {
                        span { class: "note", "{line}" }
                    }
                    if settings_marks::can_reset(&marks) {
                        button { class: "link-button text-xs", onclick: move |_| resetting.set(true), "Reset to default" }
                    }
                }
                if entry.apply == SettingApply::Restart {
                    if entry.restart_pending {
                        span { class: "note note-restart",
                            icons::RotateCcw { size: 13 }
                            {
                                let now = entry.running.as_ref().map_or_else(|| "none".to_owned(), |v| settings_marks::shown(key, v));
                                let after = entry.value.as_ref().map_or_else(|| "none".to_owned(), |v| settings_marks::shown(key, v));
                                format!("Now: {now} · After restart: {after}")
                            }
                        }
                    } else {
                        span { class: "note", "Applies after a restart" }
                    }
                }
            }
        }
        if resetting() {
            super::wizard::ResetDialog {
                label: label.clone(),
                text: settings_marks::reset_draft_text(&marks),
                on_cancel: move |_| resetting.set(false),
                on_reset: move |_| {
                    resetting.set(false);
                    // Back to what is saved if that is the default, else
                    // the runtime value taken out at Save.
                    state.put(key, Ok(None));
                },
            }
        }
    }
}

/// A box of text, a number, GiB or a secret.
#[component]
pub(super) fn TextBox(field: SchemaField) -> Element {
    let key = kept_key(&field.key);
    let label = field.label.clone();
    let widget = field.widget;
    let state = use_context::<AreaState>();
    let entry = state.entry(key);
    let secret = matches!(widget, FieldWidget::Secret | FieldWidget::EnvRef);
    let set = entry.as_ref().is_some_and(|e| e.set);
    let mut text = use_signal(|| {
        if secret {
            String::new()
        } else {
            text_of(&field, state.shown(key).as_ref())
        }
    });
    let unit = match widget {
        FieldWidget::Gib => "GiB".to_owned(),
        _ => field.unit.clone(),
    };
    let placeholder = match (&entry, widget) {
        (Some(e), FieldWidget::Secret | FieldWidget::EnvRef) if e.set => {
            "Set · type to replace".to_owned()
        }
        (_, FieldWidget::EnvRef) => {
            format!("${{{}}}", field.env_example.as_deref().unwrap_or_default())
        }
        (_, FieldWidget::Secret) => "Not set".to_owned(),
        (Some(e), _) => e
            .default
            .as_ref()
            .map(|d| text_of(&field, Some(d)))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let kind = if secret { "password" } else { "text" };
    let reads = field.clone();
    rsx! {
        div { class: "setting-control flex items-center gap-2",
            input {
                class: "setting-input",
                r#type: kind,
                aria_label: "{label}",
                value: "{text}",
                placeholder,
                autocomplete: if secret { "new-password" } else { "off" },
                inputmode: if widget == FieldWidget::Number { "numeric" } else { "text" },
                disabled: state.locked(key),
                oninput: move |e| {
                    let typed = e.value();
                    text.set(typed.clone());
                    if secret && typed.is_empty() {
                        // An empty secret box keeps the secret.
                        state.forget(key);
                    } else {
                        state.put(key, read(&reads, &typed));
                    }
                },
            }
            if !unit.is_empty() {
                span { class: "text-sm text-ctp-subtext0", "{unit}" }
            }
            if secret && set && !state.locked(key) {
                button {
                    class: "link-button text-xs",
                    r#type: "button",
                    onclick: move |_| {
                        text.set(String::new());
                        state.put(key, Ok(None));
                    },
                    "Take it out"
                }
            }
        }
    }
}

#[component]
fn SwitchBox(setting: &'static str, label: String) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let on = state.shown(key) == Some(Value::Bool(true));
    rsx! {
        div { class: "setting-control",
            button {
                class: "switch",
                role: "switch",
                r#type: "button",
                aria_checked: "{on}",
                aria_label: "{label}",
                disabled: state.locked(key),
                onclick: move |_| state.put(key, Ok(Some(Value::Bool(!on)))),
            }
        }
    }
}

#[component]
fn ChoiceBox(setting: &'static str, options: Vec<SettingOption>) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let now = state.shown(key);
    let now = now
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let locked = state.locked(key);
    rsx! {
        div { class: "setting-control flex flex-col gap-2", role: "radiogroup",
            for SettingOption { value, name, about } in options {
                button {
                    key: "{value}",
                    class: if now == value { "choice choice-here" } else { "choice" },
                    r#type: "button",
                    role: "radio",
                    aria_checked: "{now == value}",
                    disabled: locked,
                    onclick: {
                        let value = value.clone();
                        move |_| state.put(key, Ok(Some(Value::String(value.clone()))))
                    },
                    span { class: "radio" }
                    span { class: "choice-what", b { "{name}" } "{about}" }
                }
            }
        }
    }
}

#[component]
fn ManyBox(setting: &'static str, options: Vec<SettingOption>) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let shown = state.shown(key);
    // Unset is every one, in this order.
    let picked: Vec<String> = match shown {
        Some(v) => items(Some(&v)),
        None => options.iter().map(|o| o.value.clone()).collect(),
    };
    let locked = state.locked(key);
    rsx! {
        div { class: "setting-control flex flex-wrap gap-4",
            for SettingOption { value, name, .. } in options.clone() {
                label { key: "{value}", class: "flex items-center gap-2 text-sm",
                    input {
                        r#type: "checkbox",
                        checked: picked.contains(&value),
                        disabled: locked,
                        onchange: {
                            let picked = picked.clone();
                            let all: Vec<String> = options.iter().map(|o| o.value.clone()).collect();
                            move |e: FormEvent| {
                                let on = e.checked();
                                let now: Vec<String> = all
                                    .iter()
                                    .filter(|v| if **v == value { on } else { picked.contains(v) })
                                    .cloned()
                                    .collect();
                                state.put(key, Ok(Some(list_value(&now))));
                            }
                        },
                    }
                    "{name}"
                }
            }
        }
    }
}

/// A list of text, an item a row, with a box to add one.
#[component]
fn ListBox(setting: &'static str, add: String, label: String) -> Element {
    let key = setting;
    let state = use_context::<AreaState>();
    let mut rows = use_signal(|| items(state.shown(key).as_ref()));
    let mut adding = use_signal(String::new);
    let locked = state.locked(key);
    let commit = move || {
        let now = rows.peek().clone();
        state.put(key, Ok(Some(list_value(&now))));
    };
    rsx! {
        div { class: "setting-control flex flex-col gap-2",
            for (i, item) in rows().into_iter().enumerate() {
                div { key: "{i}-{item}", class: "flex items-center gap-2",
                    span { class: "mono-value min-w-0 flex-grow break-all text-sm", "{item}" }
                    button {
                        class: "icon-button",
                        r#type: "button",
                        aria_label: "Remove {item}",
                        disabled: locked,
                        onclick: move |_| {
                            rows.write().remove(i);
                            commit();
                        },
                        icons::Trash {}
                    }
                }
            }
            if !locked {
                form {
                    class: "flex items-center gap-2",
                    onsubmit: move |e: FormEvent| {
                        e.prevent_default();
                        let typed = adding.peek().trim().to_owned();
                        if !typed.is_empty() {
                            rows.write().push(typed);
                            adding.set(String::new());
                            commit();
                        }
                    },
                    input {
                        class: "setting-input",
                        aria_label: "{label}: {add}",
                        value: "{adding}",
                        oninput: move |e| adding.set(e.value()),
                    }
                    button {
                        class: "btn-dialog btn-dialog-grey",
                        r#type: "submit",
                        disabled: adding().trim().is_empty(),
                        icons::Plus {}
                        "{add}"
                    }
                }
            }
        }
    }
}
