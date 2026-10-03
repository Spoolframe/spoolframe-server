//! The app: routes and pages.

mod admin;
mod http;
mod rpc;

use dioxus::prelude::*;
use spoolframe_admin_api::UserInfo;

use admin::{AdminFrame, AdminHome, Server, UserSessions, Users};

use crate::logic::{ApiError, Landing, landing};

const STYLES: Asset = asset!("/assets/tailwind.css");

pub fn launch() {
    dioxus_logger::init(tracing::Level::INFO).expect("the logger starts once");
    dioxus::launch(App);
}

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
enum Route {
    // Signed out: no layout, which would need someone signed in.
    #[route("/")]
    Start {},
    #[route("/login")]
    Login {},
    #[route("/setup")]
    Setup {},
    #[layout(Shell)]
        #[nest("/admin")]
            #[layout(AdminFrame)]
                #[route("/")]
                AdminHome {},
                #[route("/users")]
                Users {},
                #[route("/users/:username")]
                UserSessions { username: String },
                #[route("/server")]
                Server {},
            #[end_layout]
        #[end_nest]
        #[route("/profile")]
        Profile {},
    #[end_layout]
    #[route("/:..rest")]
    NotFound { rest: Vec<String> },
}

impl From<Landing> for Route {
    fn from(landing: Landing) -> Self {
        match landing {
            Landing::Setup => Route::Setup {},
            Landing::Login => Route::Login {},
            Landing::Admin => Route::Users {},
            Landing::Profile => Route::Profile {},
        }
    }
}

#[component]
fn App() -> Element {
    rsx! {
        document::Stylesheet { href: STYLES }
        Router::<Route> {}
    }
}

/// Where this browser belongs: setup, login, or the signed-in user's pages.
async fn where_to() -> Result<Landing, ApiError> {
    let me = http::me().await?;
    let setup_open = match me {
        Some(_) => false,
        None => http::setup_open().await?,
    };
    Ok(landing(setup_open, me.map(|u| u.role)))
}

#[component]
fn Start() -> Element {
    let nav = navigator();
    let decided = use_resource(where_to);
    use_effect(move || {
        if let Some(Ok(landing)) = &*decided.read() {
            nav.replace(Route::from(*landing));
        }
    });
    match &*decided.read() {
        Some(Err(e)) => rsx! {
            Centered { Alert { text: e.explain() } }
        },
        _ => rsx! {
            Centered { p { class: "text-ctp-subtext0", "Loading…" } }
        },
    }
}

/// A page with one card in the middle.
#[component]
fn Centered(children: Element) -> Element {
    rsx! {
        main { class: "min-h-screen bg-ctp-base text-ctp-text flex items-center justify-center p-4",
            div { class: "w-full max-w-sm", {children} }
        }
    }
}

#[component]
fn Alert(text: String) -> Element {
    rsx! {
        p { class: "rounded-md border border-ctp-red/40 bg-ctp-red/10 px-3 py-2 text-sm text-ctp-red",
            role: "alert",
            "{text}"
        }
    }
}

#[component]
fn Field(
    label: String,
    value: Signal<String>,
    #[props(default = "text".to_string())] kind: String,
    #[props(default)] autocomplete: String,
) -> Element {
    let mut value = value;
    rsx! {
        label { class: "block text-sm text-ctp-subtext1",
            "{label}"
            input {
                class: "mt-1 block w-full rounded-md border border-ctp-surface1 bg-ctp-mantle px-3 py-2 text-ctp-text focus:border-ctp-mauve focus:outline-none",
                r#type: "{kind}",
                autocomplete: "{autocomplete}",
                value: "{value}",
                oninput: move |e| value.set(e.value()),
            }
        }
    }
}

#[component]
fn Submit(text: String, busy: bool) -> Element {
    rsx! {
        button {
            class: "w-full rounded-md bg-ctp-mauve px-3 py-2 font-medium text-ctp-base hover:opacity-90 disabled:opacity-50",
            r#type: "submit",
            disabled: busy,
            "{text}"
        }
    }
}

#[component]
fn Login() -> Element {
    let nav = navigator();
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        spawn(async move {
            match http::login(&username(), &password()).await {
                Ok(user) => {
                    rpc::forget();
                    nav.replace(Route::from(landing(false, Some(user.role))));
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    rsx! {
        Centered {
            form { class: "space-y-4 rounded-lg bg-ctp-mantle p-6 shadow", onsubmit: submit,
                h1 { class: "text-xl font-semibold", "Sign in to Spoolframe" }
                Field { label: "Username", value: username, autocomplete: "username" }
                Field { label: "Password", value: password, kind: "password", autocomplete: "current-password" }
                if let Some(text) = error() {
                    Alert { text }
                }
                Submit { text: "Sign in", busy: busy() }
            }
        }
    }
}

#[component]
fn Setup() -> Element {
    let nav = navigator();
    let code = use_signal(String::new);
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let again = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let submit = move |e: FormEvent| {
        e.prevent_default();
        if password() != again() {
            error.set(Some("The passwords are not the same.".into()));
            return;
        }
        busy.set(true);
        spawn(async move {
            match http::setup(&code(), &username(), &password()).await {
                Ok(_) => {
                    rpc::forget();
                    nav.replace(Route::Users {});
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    rsx! {
        Centered {
            form { class: "space-y-4 rounded-lg bg-ctp-mantle p-6 shadow", onsubmit: submit,
                h1 { class: "text-xl font-semibold", "Set up Spoolframe" }
                p { class: "text-sm text-ctp-subtext0",
                    "Create the first admin. The setup code is in the server's log."
                }
                Field { label: "Setup code", value: code, autocomplete: "one-time-code" }
                Field { label: "Username", value: username, autocomplete: "username" }
                Field { label: "Password", value: password, kind: "password", autocomplete: "new-password" }
                Field { label: "Password again", value: again, kind: "password", autocomplete: "new-password" }
                if let Some(text) = error() {
                    Alert { text }
                }
                Submit { text: "Create admin", busy: busy() }
            }
        }
    }
}

/// The signed-in pages' frame: who, and signing out. Who is asked again on
/// every load, role included.
#[component]
fn Shell() -> Element {
    let nav = navigator();
    let me = use_resource(http::me);
    let current = me.read().clone();
    match current {
        None => rsx! {
            Centered { p { class: "text-ctp-subtext0", "Loading…" } }
        },
        Some(Err(e)) => rsx! {
            Centered { Alert { text: e.explain() } }
        },
        Some(Ok(None)) => {
            nav.replace(Route::Start {});
            rsx! {}
        }
        Some(Ok(Some(user))) => rsx! {
            SignedIn { user }
        },
    }
}

#[component]
fn SignedIn(user: UserInfo) -> Element {
    let nav = navigator();
    use_context_provider(|| user.clone());
    let name = user
        .display_name
        .clone()
        .unwrap_or_else(|| user.username.clone());
    let sign_out = move |_| {
        spawn(async move {
            // Signed out either way: the cookie is cleared even when the
            // session had already ended.
            let _ = http::logout().await;
            rpc::forget();
            nav.replace(Route::Login {});
        });
    };
    rsx! {
        div { class: "min-h-screen bg-ctp-base text-ctp-text",
            header { class: "flex items-center justify-between border-b border-ctp-surface0 bg-ctp-mantle px-4 py-3",
                span { class: "font-semibold", "Spoolframe" }
                div { class: "flex items-center gap-3 text-sm",
                    span { class: "text-ctp-subtext1", "{name}" }
                    button {
                        class: "rounded-md border border-ctp-surface1 px-2 py-1 hover:bg-ctp-surface0",
                        onclick: sign_out,
                        "Sign out"
                    }
                }
            }
            main { class: "mx-auto max-w-4xl p-4", Outlet::<Route> {} }
        }
    }
}

#[component]
fn Profile() -> Element {
    let user = use_context::<UserInfo>();
    rsx! {
        h1 { class: "mb-4 text-xl font-semibold", "My profile" }
        dl { class: "grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2 rounded-lg bg-ctp-mantle p-4 text-sm",
            dt { class: "text-ctp-subtext0", "Username" }
            dd { "{user.username}" }
            dt { class: "text-ctp-subtext0", "Role" }
            dd { "{user.role}" }
        }
    }
}

#[component]
fn NotFound(rest: Vec<String>) -> Element {
    rsx! {
        Centered {
            p { "There is no page at /{rest.join(\"/\")}." }
            Link { class: "text-ctp-mauve underline", to: Route::Start {}, "Go to the start" }
        }
    }
}
