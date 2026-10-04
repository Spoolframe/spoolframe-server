//! The app: routes and pages, laid out as the "Spoolframe UI" design's
//! web boards draw them.

mod admin;
mod device;
mod http;
mod icons;
mod profile;
mod rpc;
mod webauthn;

use dioxus::prelude::*;
use spoolframe_admin_api::{MeApiClient, PasswordChange, Role, UserInfo};

use admin::{AdminHome, Server, UserSessions, Users};
use device::DeviceApproval;
use profile::Profile;

use crate::logic::{ApiError, Landing, avatar_class, landing, setup_without_passkeys};

const STYLES: Asset = asset!("/assets/tailwind.css");
/// The design's fonts, carried with the bundle (the build copies them in
/// from nixpkgs): no font is fetched from the Internet at run time.
const FONTS: Asset = asset!("/assets/fonts", AssetOptions::folder());

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
    // Opened from a device's QR code (P3-05): signs in by itself, and stays
    // on the code.
    #[route("/auth/device?:code")]
    DeviceApproval { code: String },
    #[layout(Shell)]
        #[nest("/admin")]
            #[route("/")]
            AdminHome {},
            #[route("/users")]
            Users {},
            #[route("/users/:username")]
            UserSessions { username: String },
            #[route("/server")]
            Server {},
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

/// The `@font-face` rules for [`FONTS`]: variable files, one per family.
fn font_faces() -> String {
    [
        ("Outfit", "Outfit.ttf", "100 900"),
        ("Figtree", "Figtree.ttf", "300 900"),
        ("JetBrains Mono", "JetBrainsMono.ttf", "100 800"),
        ("Bricolage Grotesque", "BricolageGrotesque.ttf", "200 800"),
    ]
    .into_iter()
    .map(|(family, file, weights)| {
        format!(
            "@font-face{{font-family:\"{family}\";src:url(\"{FONTS}/{file}\") format(\"truetype\");font-weight:{weights};font-display:swap}}"
        )
    })
    .collect()
}

#[component]
fn App() -> Element {
    rsx! {
        // The brand mark (public/, copied to the bundle's root): the SVG,
        // a PNG for browsers without SVG icons, and the home-screen icon.
        // public/favicon.ico stands in for dx serve's own at that path.
        document::Link { rel: "icon", r#type: "image/svg+xml", href: "/favicon.svg" }
        document::Link { rel: "icon", r#type: "image/png", sizes: "32x32", href: "/favicon-32.png" }
        document::Link { rel: "apple-touch-icon", href: "/apple-touch-icon.png" }
        document::Stylesheet { href: STYLES }
        document::Style { {font_faces()} }
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

/// A signed-out page: the colour blobs, and one card in the middle.
#[component]
fn Centered(children: Element) -> Element {
    rsx! {
        main { class: "relative min-h-screen overflow-hidden bg-ctp-base text-ctp-text",
            div { class: "blobs", aria_hidden: "true",
                div { class: "blob-teal" }
                div { class: "blob-blue" }
                div { class: "blob-mauve" }
            }
            div { class: "relative flex min-h-screen items-center justify-center py-10",
                {children}
            }
        }
    }
}

/// What went wrong, as the design's red box.
#[component]
fn Alert(text: String) -> Element {
    rsx! {
        div { class: "alert alert-error", role: "alert",
            icons::Warning {}
            span { "{text}" }
        }
    }
}

/// Something to know before going on: the yellow box.
#[component]
fn Notice(children: Element) -> Element {
    rsx! {
        div { class: "alert alert-warning", role: "alert",
            icons::Warning {}
            span { {children} }
        }
    }
}

/// A labelled input. `tall`: the login card's 44px (48px on a phone).
/// `error`: what is wrong with what was typed, in red under it.
#[component]
fn Field(
    label: String,
    value: Signal<String>,
    #[props(default = "text".to_string())] kind: String,
    #[props(default)] autocomplete: String,
    #[props(default)] placeholder: String,
    #[props(default)] tall: bool,
    #[props(default)] disabled: bool,
    #[props(default)] error: Option<String>,
) -> Element {
    let mut value = value;
    let invalid = error.is_some();
    let class = match (tall, invalid) {
        (true, true) => "field field-tall field-bad",
        (true, false) => "field field-tall",
        (false, true) => "field field-bad",
        (false, false) => "field",
    };
    rsx! {
        label { class,
            "{label}"
            input {
                r#type: "{kind}",
                autocomplete: "{autocomplete}",
                placeholder: "{placeholder}",
                value: "{value}",
                disabled,
                aria_invalid: invalid,
                oninput: move |e| value.set(e.value()),
            }
            if let Some(text) = error {
                span { class: "field-error", "{text}" }
            }
        }
    }
}

/// The name, as the design writes it: "spool" and, in the accent colour,
/// "frame". `class` sets its size.
#[component]
fn Wordmark(#[props(default)] class: String) -> Element {
    rsx! {
        span { class: "wordmark {class}", "spool" span { class: "text-ctp-teal", "frame" } }
    }
}

/// Now, in Unix seconds, by the browser's clock.
fn now() -> i64 {
    (js_sys::Date::now() / 1000.0) as i64
}

/// Whether the page is in a secure context: HTTPS, or `localhost`. Only
/// there does the browser keep the `Secure` session cookie, and only
/// there do passkeys work.
fn secure_context() -> bool {
    web_sys::window().is_some_and(|window| window.is_secure_context())
}

/// The server as a signed-out page knows it: its name and public URL.
async fn site() -> http::Site {
    // Without an answer the card still works, under the plain name.
    http::site().await.unwrap_or_default()
}

#[component]
fn Login() -> Element {
    let nav = navigator();
    let site = use_resource(site);
    let name = site
        .read()
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_default();
    let public_url = site.read().as_ref().and_then(|s| s.public_url.clone());
    rsx! {
        Centered {
            SignInForm {
                title: if name.is_empty() { "Sign in to Spoolframe".to_owned() } else { format!("Sign in to {name}") },
                public_url,
                on_done: move |user: http::SignedIn| {
                    nav.replace(Route::from(landing(false, Some(user.role))));
                },
            }
        }
    }
}

/// How long a passkey request fetched ahead is used: short of the 5
/// minutes the server keeps it.
const REQUEST_FRESH_SECS: i64 = 4 * 60;

/// A passkey sign-in request fetched before it is needed, and when.
async fn passkey_request() -> Option<(spoolframe_admin_api::PasskeyChallenge, i64)> {
    if !webauthn::supported() {
        return None;
    }
    // A server without passkeys says so again when the button is pressed.
    http::passkey_begin().await.ok().map(|c| (c, now()))
}

/// Sign in with a passkey: the browser's prompt, with no username. With
/// `ahead`, a request fetched earlier, the prompt opens straight from the
/// click, which some browsers insist on.
async fn passkey_sign_in(
    ahead: Option<(spoolframe_admin_api::PasskeyChallenge, i64)>,
) -> Result<http::SignedIn, String> {
    let challenge = match ahead {
        Some((challenge, at)) if now() - at < REQUEST_FRESH_SECS => challenge,
        _ => http::passkey_begin().await.map_err(|e| e.explain())?,
    };
    let signed = webauthn::get(&challenge.options).await?;
    http::passkey_finish(&challenge.ceremony, &signed)
        .await
        .map_err(|e| e.explain())
}

/// The sign-in card (the design's "Web · Login"): a password, or a
/// passkey. Used by the login page and by pages that need someone signed
/// in before they go on. In an insecure context (plain HTTP on a real
/// host) it says where to go instead and takes nothing: the browser
/// would drop the session cookie, and signing in would look like nothing.
#[component]
fn SignInForm(
    title: String,
    #[props(default)] public_url: Option<String>,
    on_done: EventHandler<http::SignedIn>,
) -> Element {
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut request = use_resource(passkey_request);
    let secure = secure_context();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        spawn(async move {
            match http::login(&username(), &password()).await {
                Ok(user) => {
                    rpc::forget();
                    on_done.call(user);
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let with_passkey = move |_| {
        busy.set(true);
        let ahead = request.read().clone().flatten();
        spawn(async move {
            match passkey_sign_in(ahead).await {
                Ok(user) => {
                    rpc::forget();
                    on_done.call(user);
                }
                Err(text) => {
                    error.set(Some(text));
                    // A request is good once: the next try needs another.
                    request.restart();
                }
            }
            busy.set(false);
        });
    };
    let elsewhere = match &public_url {
        Some(url) => format!("Open {url} instead."),
        None => "Open it over HTTPS, or as http://localhost through an SSH tunnel, instead.".into(),
    };
    rsx! {
        form { class: "glass-card", onsubmit: submit,
            div { class: "flex flex-col items-center gap-2.5",
                icons::Mark {}
                h1 { class: "m-0 text-center text-[26px] font-bold", "{title}" }
            }
            if !secure {
                div { class: "alert alert-error", role: "alert",
                    icons::Warning {}
                    span {
                        "This address cannot be used to sign in: the browser will not keep the session over plain HTTP. "
                        if let Some(url) = &public_url {
                            "Open "
                            a { href: "{url}", "{url}" }
                            " instead."
                        } else {
                            "{elsewhere}"
                        }
                    }
                }
            }
            Field { label: "Username", value: username, autocomplete: "username", placeholder: "Username", tall: true, disabled: !secure }
            Field { label: "Password", value: password, kind: "password", autocomplete: "current-password", placeholder: "Password", tall: true, disabled: !secure }
            if let Some(text) = error() {
                Alert { text }
            }
            button { class: "btn-primary", r#type: "submit", disabled: busy() || !secure, "Sign in" }
            if secure && webauthn::supported() {
                div { class: "or-divider", "or" }
                button {
                    class: "btn-secondary",
                    r#type: "button",
                    disabled: busy(),
                    onclick: with_passkey,
                    icons::Key {}
                    "Sign in with a passkey"
                }
            }
            p { class: "m-0 text-center text-xs text-ctp-subtext0",
                "Accounts are created by your server's admin."
            }
        }
    }
}

/// What a passkey made at setup is called until its user renames it.
const SETUP_PASSKEY_NAME: &str = "Passkey";

/// The design's "Web · Setup": the first admin's account, made with a
/// passkey, a password, or both. The passkey makes the account at once
/// (the server signs its admin in); a password typed before it waits for
/// it, or makes the account by itself. Where the page cannot make a
/// passkey, a password only, with a line saying why.
#[component]
fn Setup() -> Element {
    let site = use_resource(site);
    let state = use_resource(http::setup_state);
    let name = site
        .read()
        .as_ref()
        .map(|s| s.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Spoolframe".into());
    // Until the server answers, as if it could: it says so again when the
    // passkey is begun.
    let server_says = state
        .read()
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .and_then(|s| s.passkeys_unavailable.clone());
    let without = setup_without_passkeys(
        secure_context(),
        webauthn::supported(),
        server_says.as_deref(),
    );
    rsx! {
        Centered {
            div { class: "flex w-full flex-col items-center gap-[22px]",
                div { class: "flex items-center gap-2.5",
                    icons::Mark { size: 40 }
                    Wordmark { class: "text-[24px]" }
                }
                match without {
                    Some(why) => rsx! { PasswordSetup { name, why } },
                    None => rsx! { MethodsSetup { name } },
                }
                span { class: "text-xs text-ctp-overlay0",
                    "First launch · this page closes after the admin account is created"
                }
            }
        }
    }
}

/// The card's heading.
#[component]
fn SetupHeading(name: String) -> Element {
    rsx! {
        div { class: "flex flex-col gap-1.5",
            h1 { class: "m-0 font-display text-[26px] font-bold", "Create the admin account" }
            p { class: "m-0 text-sm leading-relaxed text-ctp-subtext0",
                "This account manages users, libraries and devices on "
                b { class: "text-ctp-text", "{name}" }
                ". Nobody else can open this page once it exists. The setup code is in the server's log."
            }
        }
    }
}

/// Setup with a password only, where no passkey can be made: `why`.
#[component]
fn PasswordSetup(name: String, why: String) -> Element {
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
        form { class: "glass-card glass-card-wide", onsubmit: submit,
            SetupHeading { name }
            p { class: "m-0 text-xs text-ctp-yellow", "{why}" }
            Field { label: "Setup code", value: code, autocomplete: "one-time-code", placeholder: "K7QD-M2XP-9RTB" }
            Field { label: "Username", value: username, autocomplete: "username" }
            Field { label: "Password", value: password, kind: "password", autocomplete: "new-password" }
            Field { label: "Password again", value: again, kind: "password", autocomplete: "new-password" }
            if let Some(text) = error() {
                Alert { text }
            }
            button { class: "btn-primary", r#type: "submit", disabled: busy(), "Create admin" }
        }
    }
}

/// Set the signed-in user's first password: they have none to give.
async fn first_password(password: String) -> Result<(), ApiError> {
    let client = rpc::client().await?;
    client
        .set_password(PasswordChange {
            current_password: String::new(),
            new_password: password,
        })
        .await
        .map_err(rpc::error)
}

/// Setup as the board draws it: the code and username, then the ways to
/// sign in, the passkey first.
#[component]
fn MethodsSetup(name: String) -> Element {
    let nav = navigator();
    let code = use_signal(String::new);
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let again = use_signal(String::new);
    // The account exists: its passkey made it, and the admin is signed in.
    let mut made = use_signal(|| false);
    // A password typed before the account existed, for when it does.
    let mut waiting = use_signal(|| None::<String>);
    // The account has a password.
    let mut has_password = use_signal(|| false);
    let mut typing = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let password_added = has_password() || waiting().is_some();
    let create_passkey = move |_| {
        error.set(None);
        busy.set(true);
        spawn(async move {
            let created = async {
                let challenge = http::setup_passkey_begin(&code(), &username())
                    .await
                    .map_err(|e| e.explain())?;
                let credential = webauthn::create(&challenge.options).await?;
                http::setup_passkey_finish(
                    &code(),
                    &challenge.ceremony,
                    SETUP_PASSKEY_NAME,
                    &credential,
                )
                .await
                .map_err(|e| e.explain())
            };
            match created.await {
                Ok(_) => {
                    rpc::forget();
                    made.set(true);
                    if let Some(typed) = waiting.take() {
                        match first_password(typed).await {
                            Ok(()) => has_password.set(true),
                            Err(e) => {
                                typing.set(true);
                                error.set(Some(format!(
                                    "The account is made, but the password was not set: {}",
                                    e.explain()
                                )));
                            }
                        }
                    }
                }
                Err(text) => error.set(Some(text)),
            }
            busy.set(false);
        });
    };
    let save_password = move |_| {
        if password() != again() {
            error.set(Some("The passwords are not the same.".into()));
            return;
        }
        error.set(None);
        if !made() {
            waiting.set(Some(password()));
            typing.set(false);
            return;
        }
        busy.set(true);
        spawn(async move {
            match first_password(password()).await {
                Ok(()) => {
                    has_password.set(true);
                    typing.set(false);
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let finish = move |_| {
        if made() {
            nav.replace(Route::Users {});
            return;
        }
        let Some(typed) = waiting() else {
            return;
        };
        busy.set(true);
        spawn(async move {
            match http::setup(&code(), &username(), &typed).await {
                Ok(_) => {
                    rpc::forget();
                    nav.replace(Route::Users {});
                }
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    let only_passkey = made() && !password_added;
    rsx! {
        section { class: "glass-card glass-card-setup",
            SetupHeading { name }
            div { class: "grid grid-cols-[repeat(auto-fit,minmax(200px,1fr))] gap-3",
                Field { label: "Setup code", value: code, autocomplete: "one-time-code", placeholder: "K7QD-M2XP-9RTB", disabled: made() }
                Field { label: "Username", value: username, autocomplete: "username", disabled: made() }
            }
            div { class: "flex flex-col gap-0.5",
                h2 { class: "m-0 text-base font-bold", "Ways to sign in" }
                span { class: "text-[13px] text-ctp-subtext0", "One is enough to finish. Two is safer." }
            }
            div { class: "flex flex-col gap-2",
                div { class: "method",
                    span { class: "method-icon", icons::Key { size: 19 } }
                    span { class: "method-what",
                        b { "Passkey" }
                        "On this device, a phone or a security key"
                    }
                    if made() {
                        span { class: "added", icons::CircleCheck {} "Added" }
                    } else {
                        button { class: "btn btn-teal", disabled: busy(), onclick: create_passkey, "Create a passkey" }
                    }
                }
                div { class: "method",
                    span { class: "method-icon", icons::Lock {} }
                    span { class: "method-what",
                        b { "Password" span { class: "chip", "Optional" } }
                        "Works from any browser, including ones without passkeys"
                    }
                    if password_added {
                        span { class: "added", icons::CircleCheck {} "Added" }
                    } else if !typing() {
                        button { class: "btn", onclick: move |_| typing.set(true), "Add password" }
                    }
                }
                if typing() {
                    div { class: "method-form",
                        Field { label: "Password", value: password, kind: "password", autocomplete: "new-password" }
                        Field { label: "Confirm password", value: again, kind: "password", autocomplete: "new-password" }
                        div { class: "flex gap-2",
                            button { class: "btn btn-teal h-[34px]", disabled: busy(), onclick: save_password, "Save password" }
                            button { class: "btn h-[34px]", onclick: move |_| typing.set(false), "Cancel" }
                        }
                    }
                }
            }
            if only_passkey {
                div { class: "alert alert-warning", role: "note",
                    icons::Warning {}
                    span { class: "flex flex-col gap-1",
                        b { class: "text-sm", "The passkey is your only way in" }
                        span { class: "text-ctp-text",
                            "If this device is lost or its passkeys are reset, nobody can sign in as admin, and getting back in needs shell access to the server. Add a password as well (single sign-on comes later). You can also do it later in My profile."
                        }
                    }
                }
            } else if made() {
                div { class: "alert alert-success", role: "note",
                    icons::ShieldCheck {}
                    span { "Two ways to sign in. Losing one device won't lock you out." }
                }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "flex justify-end",
                button {
                    class: "btn-dialog btn-dialog-primary",
                    disabled: busy() || !(made() || waiting().is_some()),
                    onclick: finish,
                    icons::ArrowRight {}
                    if only_passkey { "Continue with passkey only" } else { "Continue" }
                }
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

/// The first letter of a name, for the avatar.
fn initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
}

/// A role as the pages name it.
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Admin => "Admin",
        Role::Consumer => "Consumer",
        _ => "Unknown role",
    }
}

/// The frame itself (the design's sidebar, or its bar on a phone): the
/// mark, the pages, and who is signed in. On a phone an admin's pages are
/// behind the bar's menu button (the WebAdminPhone board).
#[component]
fn SignedIn(user: UserInfo) -> Element {
    let nav = navigator();
    use_context_provider(|| user.clone());
    let route = use_route::<Route>();
    let name = user
        .display_name
        .clone()
        .unwrap_or_else(|| user.username.clone());
    let admin = user.role == Role::Admin;
    let avatar = avatar_class(&user.username, false);
    let mut menu = use_signal(|| false);
    let sign_out = move |_| {
        spawn(async move {
            // Signed out either way: the cookie is cleared even when the
            // session had already ended.
            let _ = http::logout().await;
            rpc::forget();
            nav.replace(Route::Login {});
        });
    };
    let link = |to: Route, label: &'static str, divider: bool| {
        let here = match (&route, &to) {
            (Route::UserSessions { .. }, Route::Users {}) => true,
            (a, b) => a == b,
        };
        let class = match (here, divider) {
            (true, true) => "nav-link nav-link-here nav-divider",
            (true, false) => "nav-link nav-link-here",
            (false, true) => "nav-link nav-divider",
            (false, false) => "nav-link",
        };
        rsx! {
            Link { class, to, "{label}" }
        }
    };
    let title = match route {
        Route::Profile {} => "My profile",
        Route::Server {} => "Server",
        _ => "Users",
    };
    rsx! {
        // `content-start`: on a phone the bar and the page are two wrapped
        // lines, and stretched lines would leave a gap under the bar.
        div { class: "flex min-h-screen flex-wrap content-start bg-ctp-base text-ctp-text",
            aside { class: "sidebar hidden md:flex",
                div { class: "flex items-center gap-2.5 px-2",
                    icons::Reel {}
                    if admin {
                        div { class: "flex flex-col",
                            Wordmark {}
                            span { class: "text-xs text-ctp-subtext0", "Admin" }
                        }
                    } else {
                        Wordmark {}
                    }
                }
                nav { class: "flex flex-col gap-0.5",
                    if admin {
                        {link(Route::Users {}, "Users and devices", false)}
                        {link(Route::Server {}, "Server", false)}
                        {link(Route::Profile {}, "My profile", true)}
                    } else {
                        {link(Route::Profile {}, "My profile", false)}
                    }
                }
                div { class: "mt-auto flex items-center gap-2.5 rounded-[10px] bg-ctp-base p-2",
                    span { class: avatar, "{initial(&name)}" }
                    span { class: "flex min-w-0 flex-grow flex-col",
                        b { class: "truncate text-sm", "{name}" }
                        span { class: "text-xs text-ctp-subtext0", "{role_name(user.role)}" }
                    }
                    button { class: "text-ctp-subtext0 hover:text-ctp-text", aria_label: "Sign out", title: "Sign out", onclick: sign_out,
                        icons::LogOut {}
                    }
                }
            }
            header { class: "phone-bar w-full md:hidden",
                span { class: "flex items-center gap-2.5",
                    if admin {
                        button {
                            class: "icon-button -ml-1",
                            aria_label: "Menu",
                            aria_expanded: "{menu()}",
                            onclick: move |_| menu.toggle(),
                            icons::Menu {}
                        }
                    }
                    icons::Reel { size: 24 }
                    b { class: "font-display text-[17px]", "{title}" }
                }
                span { class: "flex items-center gap-3",
                    button { class: "text-ctp-subtext0", aria_label: "Sign out", onclick: sign_out, icons::LogOut {} }
                    span { class: "{avatar} avatar-md", "{initial(&name)}" }
                }
            }
            if admin && menu() {
                div { class: "drawer-scrim md:hidden", aria_hidden: "true", onclick: move |_| menu.set(false) }
                // A link's click comes up here once it has navigated.
                nav { class: "drawer md:hidden", aria_label: "Admin", onclick: move |_| menu.set(false),
                    {link(Route::Users {}, "Users and devices", false)}
                    {link(Route::Server {}, "Server", false)}
                    {link(Route::Profile {}, "My profile", true)}
                }
            }
            main { class: "box-border min-w-0 flex-[1_1_600px] max-w-[1200px] p-3.5 md:px-10 md:py-8",
                Outlet::<Route> {}
            }
        }
    }
}

/// The design's "Web · Not found".
#[component]
fn NotFound(rest: Vec<String>) -> Element {
    let _ = rest;
    rsx! {
        Centered {
            section { class: "glass-card",
                div { class: "flex flex-col items-center gap-3 text-center",
                    icons::Mark {}
                    span { class: "font-mono text-sm text-ctp-subtext0", "404" }
                    h1 { class: "m-0 text-2xl font-bold", "There's no page here" }
                    p { class: "m-0 text-sm text-ctp-subtext1", "The address may be mistyped, or the page moved." }
                    Link { class: "btn-primary btn-start", to: Route::Start {}, "Go to the start" }
                }
            }
        }
    }
}
