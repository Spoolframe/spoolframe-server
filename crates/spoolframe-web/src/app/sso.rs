//! Settings › Sign-in providers (PX-22): the OpenID Connect providers
//! people may link and sign in with (P3-04), added, changed and removed
//! here, and where each one's client secret comes from. `auth.oidc` is
//! one setting, saved whole; a pasted secret goes to the server on its
//! own (`ssoSetClientSecret`), never into the settings. The form's logic
//! is `crate::providers`, tested on the host.

use dioxus::prelude::*;
use spoolframe_admin_api::{AdminApiClient, Settings};

use super::profile::{ConfirmItsYou, asks_to_confirm};
use super::wizard::{ListField, Note, entry, load_settings, loading, locked, text, value};
use super::{Alert, Field, Route, icons, rpc};
use crate::logic::{ApiError, list_setting};
use crate::providers::{
    ENV_PREFIX, Provider, SecretFrom, callback_url, entries, providers, secret_steps,
};

/// What the confirm dialog is about, for every change here.
const ABOUT: &str = "changing sign-in providers";

/// Save `values` as `auth.oidc`, deleting `forget`'s pasted secret first
/// and keeping `paste` (provider, secret) after. A pasted secret is kept
/// under its provider's name, so it is deleted while the provider still
/// has one, before the setting changes. `Err` says whether a secret was
/// deleted before the failure.
async fn change(
    settings: &Settings,
    values: Vec<serde_json::Value>,
    forget: Option<String>,
    paste: Option<(String, String)>,
) -> Result<(), (ApiError, bool)> {
    let client = rpc::client().await.map_err(|e| (e, false))?;
    let forgot = forget.is_some();
    if let Some(name) = forget {
        client
            .sso_set_client_secret(name, None)
            .await
            .map_err(|e| (rpc::error(e), false))?;
    }
    super::wizard::save(
        settings,
        vec![(
            "auth.oidc",
            list_setting(values, entry(settings, "auth.oidc")),
        )],
    )
    .await
    .map_err(|e| (e, forgot))?;
    if let Some((name, secret)) = paste {
        client
            .sso_set_client_secret(name, Some(secret))
            .await
            .map_err(|e| (rpc::error(e), false))?;
    }
    Ok(())
}

/// What a failed change says: the server's words, and that a secret
/// deleted on the way has to be pasted again.
fn change_error(e: &ApiError, forgot: bool) -> String {
    if forgot {
        format!(
            "{} The secret pasted earlier was deleted on the way: paste it again, or choose where it comes from.",
            e.explain()
        )
    } else {
        e.explain()
    }
}

#[component]
pub fn SsoForm(on_done: EventHandler<()>) -> Element {
    let advanced = super::settings::show_advanced();
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
            SsoFields {
                key: "{g}",
                settings: loaded.clone(),
                advanced,
                on_done,
                on_reload: move |_| generation += 1,
            }
        }
    }
}

/// Which provider the form is open on.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Open {
    Adding,
    Editing(usize),
}

#[component]
fn SsoFields(
    settings: Settings,
    advanced: bool,
    on_done: EventHandler<()>,
    on_reload: EventHandler<()>,
) -> Element {
    let list = providers(value(&settings, "auth.oidc"));
    let fixed = locked(&settings, "auth.oidc");
    let callback = callback_url(&text(&settings, "admin.public_url"));
    let mut open = use_signal(|| None::<Open>);
    let mut removing = use_signal(|| None::<usize>);
    rsx! {
        div { class: "flex flex-col gap-1.5",
            h2 { class: "m-0 text-[22px] font-semibold", "Sign-in providers" }
            p { class: "m-0 text-sm text-ctp-subtext0",
                "People link an account at one of these in My profile, then sign in with it: Pocket ID, Authentik, Kanidm, Keycloak, Google and others that speak OpenID Connect."
            }
        }
        div { class: "alert alert-info", role: "note",
            icons::Info {}
            span {
                b { "Saved as plain text" }
                "What is saved here is written to the server's settings file, runtime.ucl in its state directory, as plain text. That is why a client secret should come from an environment variable: the file then holds only the variable's name, never the secret."
            }
        }
        match &callback {
            Some(url) => rsx! {
                section { class: "card",
                    div { class: "card-body",
                        span { class: "text-sm", "Register this callback address at each provider:" }
                        div { class: "flex flex-wrap items-center gap-2.5",
                            span { class: "mono-value min-w-0 break-all", "{url}" }
                            super::admin::CopyButton { text: url.clone() }
                        }
                        span { class: "text-xs text-ctp-subtext0", "It is the web UI's public address, set on the Access tab, with /auth/oidc/callback after it." }
                    }
                }
            },
            None => rsx! {
                div { class: "alert alert-warning", role: "note",
                    icons::Warning {}
                    span {
                        b { "Sign-in providers need the public address" }
                        "A provider sends people back to the web UI's public address, so set it first. "
                        Link { to: Route::Settings { tab: "access".into() }, "Set it on the Access tab" }
                        "."
                    }
                }
            },
        }
        section { class: "card",
            div { class: "card-head",
                icons::Globe { size: 18 }
                h2 { "Providers" }
                span { class: "text-[13px] text-ctp-subtext0", "Changes ask you to confirm it's you if you signed in over 10 minutes ago." }
            }
            div { class: "card-body",
                if list.is_empty() {
                    p { class: "m-0 text-sm text-ctp-subtext0", "No sign-in providers yet." }
                }
                for (i, p) in list.iter().cloned().enumerate() {
                    div { key: "{p.name}", class: "row flex-wrap",
                        span { class: "row-what",
                            b { "{p.name}" }
                            span { class: "row-sub break-all", "{p.issuer}" }
                            span { class: if p.from == SecretFrom::Pasted { "row-sub text-ctp-yellow" } else { "row-sub" }, "{p.secret_line()}" }
                        }
                        if !fixed {
                            span { class: "row-do",
                                button { class: "btn", onclick: move |_| open.set(Some(Open::Editing(i))),
                                    icons::Pencil { size: 14 }
                                    "Edit"
                                }
                                button { class: "btn-delete", aria_label: "Remove {p.name}", onclick: move |_| removing.set(Some(i)),
                                    icons::Trash { size: 14 }
                                    "Remove"
                                }
                            }
                        }
                    }
                }
                Note { settings: settings.clone(), setting: "auth.oidc", on_changed: move |_| on_reload.call(()) }
                if !fixed && open().is_none() {
                    div {
                        button { class: "btn", onclick: move |_| open.set(Some(Open::Adding)),
                            icons::Plus { size: 14 }
                            "Add a provider"
                        }
                    }
                }
            }
        }
        if let Some(at) = open() {
            ProviderForm {
                key: "{at:?}",
                settings: settings.clone(),
                list: list.clone(),
                index: match at {
                    Open::Adding => None,
                    Open::Editing(i) => Some(i),
                },
                advanced,
                on_cancel: move |_| open.set(None),
                on_done,
            }
        }
        if let Some(i) = removing() {
            RemoveProvider {
                settings: settings.clone(),
                list: list.clone(),
                index: i,
                on_cancel: move |_| removing.set(None),
                on_done,
            }
        }
    }
}

/// A provider's form, for one added (`index` `None`) or one of `list`.
#[component]
fn ProviderForm(
    settings: Settings,
    list: Vec<Provider>,
    index: Option<usize>,
    advanced: bool,
    on_cancel: EventHandler<()>,
    on_done: EventHandler<()>,
) -> Element {
    let old = index.and_then(|i| list.get(i).cloned());
    let start = old.clone().unwrap_or_else(Provider::new);
    let name = use_signal(|| start.name.clone());
    let issuer = use_signal(|| start.issuer.clone());
    let client_id = use_signal(|| start.client_id.clone());
    let scopes = use_signal(|| start.scopes.clone());
    let mut from = use_signal(|| start.from);
    let env = use_signal(|| start.env.clone());
    let file = use_signal(|| start.file.clone());
    let mut secret = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let s = settings.clone();
    let before = old.clone();
    let on_save = use_callback(move |()| {
        let s = s.clone();
        let new = Provider {
            name: name(),
            issuer: issuer(),
            client_id: client_id(),
            scopes: scopes(),
            from: from(),
            env: env(),
            file: file(),
        };
        let steps = match secret_steps(before.as_ref(), Some(&new), &secret()) {
            Ok(steps) => steps,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        let mut all = list.clone();
        match index {
            Some(i) if i < all.len() => all[i] = new.clone(),
            _ => all.push(new.clone()),
        }
        let values = match entries(&all) {
            Ok(values) => values,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        let paste = steps
            .paste
            .map(|secret| (new.name.trim().to_owned(), secret));
        busy.set(true);
        spawn(async move {
            match change(&s, values, steps.forget, paste).await {
                Ok(()) => on_done.call(()),
                Err((e, _)) if asks_to_confirm(&e, confirming, ABOUT) => {}
                Err((e, forgot)) => error.set(Some(change_error(&e, forgot))),
            }
            busy.set(false);
        });
    });
    let was_pasted = old.as_ref().is_some_and(|o| o.from == SecretFrom::Pasted);
    let was_config = start.from == SecretFrom::ConfigFile;
    // A file, or scopes of its own, only with the advanced settings shown,
    // unless the provider has them already.
    let file_shown = advanced || start.from == SecretFrom::File;
    let scopes_shown = advanced || start.custom_scopes();
    let choice = |here: bool| if here { "choice choice-here" } else { "choice" };
    let reference = crate::providers::env_reference(&env());
    rsx! {
        section { class: "card",
            div { class: "card-head",
                h2 {
                    match &old {
                        Some(p) => rsx! { "Edit {p.name}" },
                        None => rsx! { "Add a provider" },
                    }
                }
            }
            div { class: "card-body form gap-[18px]",
                div { class: "flex flex-col gap-1.5",
                    Field { label: "Name", value: name, placeholder: "Pocket ID" }
                    span { class: "text-xs text-ctp-subtext0", "Up to 40 characters. The sign-in button says “Continue with” it." }
                }
                div { class: "flex flex-col gap-1.5",
                    Field { label: "Issuer", value: issuer, kind: "url", placeholder: "https://id.example.org" }
                    span { class: "text-xs text-ctp-subtext0", "The provider's address, as its tokens name it. HTTPS: the server fetches the provider's keys from it." }
                }
                Field { label: "Client ID", value: client_id, autocomplete: "off" }
                div { class: "flex flex-col gap-2",
                    h2 { "Client secret" }
                    if was_config {
                        button { class: choice(from() == SecretFrom::ConfigFile), r#type: "button", disabled: true,
                            span { class: "radio" }
                            span { class: "choice-what",
                                b { "Set in the server's config file" }
                                "The secret is written in the config file, and this page cannot copy it. To change anything here, choose where the secret comes from below."
                            }
                        }
                    }
                    button { class: choice(from() == SecretFrom::Env), r#type: "button", onclick: move |_| from.set(SecretFrom::Env),
                        span { class: "radio" }
                        span { class: "choice-what",
                            span { class: "flex flex-wrap items-center gap-2",
                                b { "Environment variable" }
                                span { class: "pill", "Recommended" }
                            }
                            "The secret stays in the server's environment. The settings keep only the variable's name."
                        }
                    }
                    if from() == SecretFrom::Env {
                        div { class: "flex flex-col gap-1.5 pl-4",
                            Field {
                                label: "Variable name",
                                value: env,
                                placeholder: "OIDC_SECRET",
                                autocomplete: "off",
                                error: if env().trim().is_empty() { None } else { reference.clone().err() },
                            }
                            span { class: "text-xs text-ctp-subtext0",
                                "Set "
                                span { class: "mono-value text-xs",
                                    {match &reference {
                                        Ok(r) => r.trim_start_matches("${").trim_end_matches('}').to_owned(),
                                        Err(_) => format!("{ENV_PREFIX}<NAME>"),
                                    }}
                                }
                                " to the secret in the server's environment: the container's, or the service's. The server reads it when it starts, and reads only variables whose names start with {ENV_PREFIX}, so {ENV_PREFIX} is added to the name typed here. Saving checks the variable is set."
                            }
                        }
                    }
                    button { class: choice(from() == SecretFrom::Pasted), r#type: "button", onclick: move |_| from.set(SecretFrom::Pasted),
                        span { class: "radio" }
                        span { class: "choice-what",
                            b { "Paste the secret" }
                            "The server keeps it in a file of its own, apart from the settings."
                        }
                    }
                    if from() == SecretFrom::Pasted {
                        div { class: "flex flex-col gap-2.5 pl-4",
                            // Before the field, and not a tooltip: whoever
                            // pastes a secret reads this first.
                            div { class: "alert alert-warning", role: "note",
                                icons::ShieldAlert {}
                                span {
                                    b { "A pasted secret is saved unencrypted" }
                                    "The server saves it as plain text, in a file in its state directory on its disk. Anyone who can read that directory, or a backup of it, can read the secret. An environment variable is strongly recommended instead."
                                }
                            }
                            // In a form of its own, as browsers expect of a
                            // password field; Enter does nothing.
                            form { class: "contents", onsubmit: move |e: FormEvent| e.prevent_default(),
                                label { class: "field",
                                    "Client secret"
                                    input {
                                        r#type: "password",
                                        autocomplete: "off",
                                        placeholder: if was_pasted { "Kept · paste a new one to replace it" } else { "" },
                                        value: "{secret}",
                                        oninput: move |e| secret.set(e.value()),
                                    }
                                }
                            }
                            if was_pasted {
                                span { class: "text-xs text-ctp-subtext0", "Left empty, the secret kept now stays. Renaming the provider needs it pasted again: it is kept under the name." }
                            }
                        }
                    }
                    if file_shown {
                        button { class: choice(from() == SecretFrom::File), r#type: "button", onclick: move |_| from.set(SecretFrom::File),
                            span { class: "radio" }
                            span { class: "choice-what",
                                b { "File on the server" }
                                "A file holding only the secret, such as a Docker or systemd secret. The server's user has to be able to read it."
                            }
                        }
                        if from() == SecretFrom::File {
                            div { class: "flex flex-col gap-1.5 pl-4",
                                Field { label: "Path on the server", value: file, placeholder: "/run/secrets/oidc", autocomplete: "off" }
                            }
                        }
                    }
                    if was_pasted && from() != SecretFrom::Pasted {
                        span { class: "text-xs text-ctp-subtext0", "Saving deletes the secret pasted earlier from the server's disk." }
                    }
                }
                if scopes_shown {
                    div { class: "flex flex-col gap-2",
                        h2 { "Scopes" }
                        span { class: "text-xs text-ctp-subtext0", "What is asked of the provider. Default: openid profile email; openid is asked for whether it is listed or not." }
                        ListField { add: "Add a scope", items: scopes, placeholder: "groups", disabled: false }
                    }
                }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "wizard-foot",
                    span { class: "wizard-foot-end",
                        button { class: "btn-dialog btn-dialog-grey", disabled: busy(), onclick: move |_| on_cancel.call(()), "Cancel" }
                        button { class: "btn-dialog btn-dialog-primary", disabled: busy(), onclick: move |_| on_save.call(()), "Save" }
                    }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| on_save.call(()) }
    }
}

/// "Remove <name>?", then the list saved without it, its pasted secret
/// deleted first.
#[component]
fn RemoveProvider(
    settings: Settings,
    list: Vec<Provider>,
    index: usize,
    on_cancel: EventHandler<()>,
    on_done: EventHandler<()>,
) -> Element {
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let confirming = use_signal(|| None::<String>);
    let gone = list.get(index).cloned().unwrap_or_default();
    let name = gone.name.clone();
    let go = use_callback(move |()| {
        let s = settings.clone();
        let mut rest = list.clone();
        if index < rest.len() {
            rest.remove(index);
        }
        let values = match entries(&rest) {
            Ok(values) => values,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        let forget = secret_steps(Some(&gone), None, "")
            .ok()
            .and_then(|steps| steps.forget);
        busy.set(true);
        spawn(async move {
            match change(&s, values, forget, None).await {
                Ok(()) => on_done.call(()),
                Err((e, _)) if asks_to_confirm(&e, confirming, ABOUT) => {}
                Err((e, forgot)) => error.set(Some(change_error(&e, forgot))),
            }
            busy.set(false);
        });
    });
    rsx! {
        div { class: "scrim",
            section { class: "dialog w-[440px]", role: "dialog",
                div { class: "dialog-head dialog-head-danger",
                    icons::Trash { size: 22 }
                    h1 { "Remove {name}?" }
                }
                p { "Nobody can sign in with {name} any more, and its button goes from the sign-in page. Accounts linked to it stay, with their other ways to sign in." }
                if let Some(text) = error() {
                    Alert { text }
                }
                div { class: "dialog-actions",
                    button { class: "btn-dialog btn-dialog-grey", onclick: move |_| on_cancel.call(()), "Cancel" }
                    button { class: "btn-dialog btn-dialog-outline text-ctp-red", disabled: busy(), onclick: move |_| go.call(()), "Remove" }
                }
            }
        }
        ConfirmItsYou { asking: confirming, on_confirmed: move |_| go.call(()) }
    }
}
