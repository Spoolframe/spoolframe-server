//! The app: routes and pages, laid out as the "Spoolframe UI" design's
//! web boards draw them.

mod activity;
mod admin;
mod catalog;
mod catalog_debug;
mod catalog_item;
mod device;
mod http;
mod icons;
mod invites;
mod libraries;
mod logs;
mod overview;
mod playback;
mod plex;
mod profile;
mod remote;
mod restart;
mod rpc;
mod settings;
mod telemetry;
mod webauthn;
mod wizard;

use dioxus::prelude::*;
use spoolframe_admin_api::{MeApiClient, PasswordChange, Role, UserInfo};

use activity::ActivityPage;
use admin::{Server, UserSessions, Users};
use catalog::CatalogLibrary;
use catalog_item::CatalogItem;
use device::DeviceApproval;
use invites::Invites;
use libraries::{AddLibrary, Libraries};
use logs::LogsPage;
use overview::AdminHome;
use playback::{PlaybackDetail, PlaybackPage};
use plex::PlexSignedIn;
use profile::Profile;
use settings::{Settings, SettingsHome};
use wizard::{Mode, Steps, WizardLayout};

use crate::logic::{ApiError, Landing, avatar_class, landing, setup_without_passkeys};
use crate::setup;

const STYLES: Asset = asset!("/assets/tailwind.css");
/// The design's fonts, carried with the bundle (the build copies them in
/// from nixpkgs): no font is fetched from the Internet at run time.
const FONTS: Asset = asset!("/assets/fonts", AssetOptions::folder());

pub fn launch() {
    dioxus_logger::init(tracing::Level::INFO).expect("the logger starts once");
    http::capture_sso_outcome();
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
    // The wizard's steps after the first admin (P3-10); signed in.
    #[route("/setup/:step")]
    Wizard { step: String },
    // Opened from a device's QR code (P3-05): signs in by itself, and stays
    // on the code.
    #[route("/auth/device?:code")]
    DeviceApproval { code: String },
    // Where Plex's sign-in ends (P3-10, P3-12): the window it opened in,
    // which closes itself; the page that opened it is the one waiting.
    // The server's `forwardUrl` names this path.
    #[route("/auth/plex")]
    PlexSignedIn {},
    #[layout(Shell)]
        #[nest("/admin")]
            #[route("/")]
            AdminHome {},
            #[route("/users")]
            Users {},
            #[route("/users/:username")]
            UserSessions { username: String },
            // Invites (P6-01): under Users and devices in the sidebar.
            #[route("/invites")]
            Invites {},
            #[route("/server")]
            Server {},
            // Server's sub-pages (P3-14): what it does now, and its log.
            #[route("/activity")]
            ActivityPage {},
            #[route("/logs")]
            LogsPage {},
            // What plays now, and one playback (P3-15).
            #[route("/playback")]
            PlaybackPage {},
            #[route("/playback/:id")]
            PlaybackDetail { id: String },
            #[route("/libraries")]
            Libraries {},
            // `back`: where Cancel and the back link go (the wizard's Plex
            // step, or Settings › Plex); Libraries when empty.
            #[route("/libraries/add?:back")]
            AddLibrary { back: String },
            // A library's items (P3-16): where the table is is in the
            // address, so a reload or a link opens the same rows.
            #[route("/libraries/:library?:page&:per&:filter&:kind&:q")]
            CatalogLibrary { library: String, page: String, per: String, filter: String, kind: String, q: String },
            #[route("/libraries/:library/:id?:tab")]
            CatalogItem { library: String, id: String, tab: String },
            #[route("/settings")]
            SettingsHome {},
            #[route("/settings/:tab")]
            Settings { tab: String },
        #[end_nest]
        #[route("/profile")]
        Profile {},
    #[end_layout]
    #[route("/:..rest")]
    NotFound { rest: Vec<String> },
}

impl From<Landing> for Route {
    fn from(landing: Landing) -> Self {
        let path = landing.path();
        match path.parse() {
            Ok(Route::NotFound { .. }) | Err(_) => {
                unreachable!("a landing's path is one of the routes: {path}")
            }
            Ok(route) => route,
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

/// Where this browser belongs: setup, login, the wizard where an admin
/// left it, or the signed-in user's pages.
async fn where_to() -> Result<Route, ApiError> {
    let me = http::me().await?;
    let setup_open = match me {
        Some(_) => false,
        None => http::setup_open().await?,
    };
    let landing = landing(setup_open, me.map(|u| u.role));
    if landing == Landing::Admin {
        let state = wizard::load_setup().await?;
        if !state.finished {
            let mut done = state.done;
            done.push("admin".into());
            let at = setup::resume(&done);
            // Straight after the first admin, nothing to welcome back to.
            let step = if at.id == "server" { at.id } else { "resume" };
            return Ok(Route::Wizard {
                step: step.to_owned(),
            });
        }
    }
    Ok(Route::from(landing))
}

#[component]
fn Start() -> Element {
    let nav = navigator();
    let decided = use_resource(where_to);
    use_effect(move || {
        if let Some(Ok(route)) = &*decided.read() {
            nav.replace(route.clone());
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

/// Now, in Unix milliseconds, by the browser's clock.
fn now_ms() -> i64 {
    js_sys::Date::now() as i64
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
    let insecure_cookies = site.read().as_ref().is_some_and(|s| s.insecure_cookies);
    let sso = site
        .read()
        .as_ref()
        .map(|s| s.sso.clone())
        .unwrap_or_default();
    // What a sign-in provider's callback sent the browser here to say,
    // naming the provider once the server says it has one by that name
    // (A-16).
    let outcome = use_hook(|| http::sso_outcome("/login"));
    let said = outcome.and_then(|(what, provider)| {
        crate::sso::login_message(&what, provider.as_deref(), &sso).map(|(_, text)| text)
    });
    rsx! {
        Centered {
            SignInForm {
                title: if name.is_empty() { "Sign in to Spoolframe".to_owned() } else { format!("Sign in to {name}") },
                public_url,
                insecure_cookies,
                sso,
                said,
                // The start decides: an admin may have a wizard to finish.
                on_done: move |_: http::SignedIn| {
                    nav.replace(Route::Start {});
                },
            }
        }
    }
}

/// How long a passkey request fetched ahead is used: short of the 5
/// minutes the server keeps it.
const REQUEST_FRESH_SECS: i64 = 4 * 60;

/// A passkey sign-in request fetched before it is needed, and when.
pub(super) async fn passkey_request() -> Option<(spoolframe_admin_api::PasskeyChallenge, i64)> {
    if !webauthn::supported() {
        return None;
    }
    // A server without passkeys says so again when the button is pressed.
    http::passkey_begin().await.ok().map(|c| (c, now()))
}

/// A passkey prompt opened from a click, before anything is awaited, if
/// `ahead` holds a request still fresh: Safari opens a prompt only from
/// the click itself, not after a network request. `None` if there is no
/// such request; the click then fetches one ([`passkey_sign_in`]).
pub(super) fn open_passkey_prompt(
    ahead: Option<(spoolframe_admin_api::PasskeyChallenge, i64)>,
) -> Option<Result<(spoolframe_admin_api::PasskeyChallenge, webauthn::Prompt), String>> {
    let (challenge, at) = ahead?;
    if now() - at >= REQUEST_FRESH_SECS {
        return None;
    }
    Some(webauthn::open_get(&challenge.options).map(|prompt| (challenge, prompt)))
}

/// What a passkey signed, and the request it answers: `opened` by
/// [`open_passkey_prompt`], or a request fetched now and its prompt.
pub(super) async fn passkey_signature(
    opened: Option<Result<(spoolframe_admin_api::PasskeyChallenge, webauthn::Prompt), String>>,
) -> Result<(spoolframe_admin_api::PasskeyChallenge, serde_json::Value), String> {
    let (challenge, prompt) = match opened {
        Some(opened) => opened?,
        None => {
            let challenge = http::passkey_begin().await.map_err(|e| e.explain())?;
            let prompt = webauthn::open_get(&challenge.options)?;
            (challenge, prompt)
        }
    };
    Ok((challenge, prompt.answer().await?))
}

/// Sign in with a passkey: the browser's prompt, with no username, as
/// [`passkey_signature`] gets it.
async fn passkey_sign_in(
    opened: Option<Result<(spoolframe_admin_api::PasskeyChallenge, webauthn::Prompt), String>>,
) -> Result<http::SignedIn, String> {
    let (challenge, signed) = passkey_signature(opened).await?;
    http::passkey_finish(&challenge.ceremony, &signed)
        .await
        .map_err(|e| e.explain())
}

/// The sign-in card (the design's "Web · Login"): a password, or a
/// passkey. Used by the login page and by pages that need someone signed
/// in before they go on. In an insecure context (plain HTTP on a real
/// host) it says where to go instead and takes nothing: the browser
/// would drop the session cookie, and signing in would look like nothing.
/// Unless the server sends insecure cookies (P3-10): then a password
/// works there, and only passkeys need the secure context.
///
/// `sso`: the sign-in providers, one "Continue with" button each (P3-04),
/// which leave for the provider; it sends the browser back signed in.
/// `said`: why the page was opened, shown at its top (the provider's
/// account is not linked, …). `return_to`: the page a provider's sign-in
/// comes back to (path and query), where it is not the start page.
/// `lede`, `code`, `submit_label` and `footer`: the approval page's own
/// words (WebApproveStates 2): what signing in is for, the code it comes
/// back to, and its button.
#[component]
fn SignInForm(
    title: String,
    #[props(default)] lede: Option<String>,
    #[props(default)] code: Option<String>,
    #[props(default)] submit_label: Option<String>,
    #[props(default)] footer: Option<String>,
    #[props(default)] public_url: Option<String>,
    #[props(default)] insecure_cookies: bool,
    #[props(default)] sso: Vec<String>,
    #[props(default)] said: Option<String>,
    #[props(default)] return_to: Option<String>,
    on_done: EventHandler<http::SignedIn>,
) -> Element {
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let mut error = use_signal(|| said.clone());
    let mut busy = use_signal(|| false);
    let mut request = use_resource(passkey_request);
    let passkeys = secure_context();
    let secure = passkeys || insecure_cookies;
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
        let opened = open_passkey_prompt(request.read().clone().flatten());
        spawn(async move {
            match passkey_sign_in(opened).await {
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
    let continue_with = use_callback(move |provider: String| {
        busy.set(true);
        let return_to = return_to.clone();
        spawn(async move {
            match http::sso_begin(&provider, "signIn", return_to.as_deref()).await {
                // Busy until the page is left.
                Ok(url) => http::go(&url),
                Err(e) => {
                    error.set(Some(e.explain()));
                    busy.set(false);
                }
            }
        });
    });
    let elsewhere = match &public_url {
        Some(url) => format!("Open {url} instead."),
        None => "Open it over HTTPS, or as http://localhost through an SSH tunnel, instead.".into(),
    };
    rsx! {
        form { class: "glass-card", onsubmit: submit,
            if code.is_some() {
                div { class: "approve-head",
                    icons::Mark { size: 48 }
                    h1 { "{title}" }
                    if let Some(lede) = &lede {
                        p { "{lede}" }
                    }
                }
            } else {
                div { class: "flex flex-col items-center gap-2.5",
                    icons::Mark {}
                    h1 { class: "m-0 text-center text-[26px] font-bold", "{title}" }
                    if let Some(lede) = &lede {
                        p { class: "m-0 text-center text-sm text-ctp-subtext0", "{lede}" }
                    }
                }
            }
            if let Some(code) = &code {
                div { class: "approve-shown",
                    span { "Code on the screen" }
                    b { class: "approve-code-shown", "{code}" }
                }
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
            if let Some(text) = error() {
                Alert { text }
            }
            Field { label: "Username", value: username, autocomplete: "username", placeholder: "Username", tall: true, disabled: !secure }
            Field { label: "Password", value: password, kind: "password", autocomplete: "current-password", placeholder: "Password", tall: true, disabled: !secure }
            button { class: "btn-primary", r#type: "submit", disabled: busy() || !secure,
                {submit_label.clone().unwrap_or_else(|| "Sign in".to_owned())}
            }
            if passkeys && webauthn::supported() {
                if code.is_none() {
                    div { class: "or-divider", "or" }
                }
                button {
                    class: "btn-secondary",
                    r#type: "button",
                    disabled: busy(),
                    onclick: with_passkey,
                    icons::Key {}
                    "Sign in with a passkey"
                }
            }
            if !sso.is_empty() && !(passkeys && webauthn::supported()) {
                div { class: "or-divider", "or" }
            }
            for provider in sso.iter().cloned() {
                button {
                    class: "btn-outline",
                    r#type: "button",
                    disabled: busy() || !secure,
                    onclick: {
                        let provider = provider.clone();
                        move |_| continue_with.call(provider.clone())
                    },
                    icons::ShieldCheck { size: 16 }
                    "Continue with {provider}"
                }
            }
            if let Some(footer) = &footer {
                p { class: "m-0 text-center text-xs text-ctp-subtext0", "{footer}" }
            } else {
                p { class: "m-0 text-center text-xs text-ctp-subtext0",
                    "Accounts are created by your server's admin."
                    if !sso.is_empty() {
                        " A sign-in provider only works once it is linked in My profile."
                    }
                }
            }
        }
    }
}

/// What a passkey made at setup is called until its user renames it.
const SETUP_PASSKEY_NAME: &str = "Passkey";

/// How long typing the username pauses before its passkey request is
/// fetched: one request per name typed, not per letter.
const SETUP_REQUEST_PAUSE_MS: i32 = 400;

/// The wizard's first page and step 1 (P3-01, P3-10): the setup code,
/// with insecure cookies offered where the page is plain HTTP; then the
/// design's "Web · Setup", the first admin's account, made with a passkey,
/// a password, or both. The passkey makes the account at once (the server
/// signs its admin in); a password typed before it waits for it, or makes
/// the account by itself. Where the page cannot make a passkey, a password
/// only, with a line saying why.
#[component]
fn Setup() -> Element {
    let site = use_resource(site);
    let state = use_resource(http::setup_state);
    let mut checked = use_signal(|| None::<String>);
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
        WizardLayout {
            match checked() {
                None => rsx! { CodeCard { name, on_ok: move |code| checked.set(Some(code)) } },
                Some(code) => rsx! {
                    Steps { current: "admin", done: Vec::new() }
                    match without {
                        Some(why) => rsx! { PasswordSetup { name, why, setup_code: code } },
                        None => rsx! { MethodsSetup { name, setup_code: code } },
                    }
                },
            }
        }
    }
}

/// The address this page is open at, as the browser shows it.
fn page_address() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

/// The wizard's first page (the WebSetupCodeHttp board): the setup code
/// from the log, and over plain HTTP, the offer of insecure cookies.
#[component]
fn CodeCard(name: String, on_ok: EventHandler<String>) -> Element {
    let mut code = use_signal(String::new);
    let mut insecure = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let plain = !secure_context();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        busy.set(true);
        spawn(async move {
            match http::setup_check(&code(), plain && insecure()).await {
                Ok(()) => on_ok.call(code()),
                Err(e) => error.set(Some(e.explain())),
            }
            busy.set(false);
        });
    };
    rsx! {
        form { class: "wizard-card", onsubmit: submit,
            div { class: "flex flex-col gap-1.5",
                h1 { "Set up {name}" }
                p { class: "lede", "Prove you run this server. The setup code is in the server's log; restarting the server makes a new one." }
            }
            input {
                class: "code-input",
                aria_label: "Setup code",
                autocomplete: "one-time-code",
                placeholder: "XXXX-XXXX-XXXX",
                value: "{code}",
                oninput: move |e| code.set(e.value()),
            }
            if plain {
                div { class: "alert alert-warning flex-col", role: "note",
                    div { class: "flex gap-2.5",
                        icons::Warning {}
                        span { class: "text-ctp-text",
                            b { "This page is open over plain HTTP" }
                            "At "
                            span { class: "mono-value", "{page_address()}" }
                            " the browser won't keep you signed in, so setup can't finish here. Open the web UI over HTTPS (through your reverse proxy), or on the server itself at localhost, or allow insecure cookies."
                        }
                    }
                    div { class: "panel flex-row items-start",
                        button {
                            class: "switch mt-0.5",
                            r#type: "button",
                            role: "switch",
                            aria_checked: "{insecure()}",
                            aria_label: "Allow insecure cookies",
                            onclick: move |_| insecure.toggle(),
                        }
                        span { class: "choice-what",
                            b { "Allow insecure cookies" }
                            "The session cookie crosses your network unencrypted. Anyone on the network who can see the traffic could take over the session. Passkeys still need HTTPS or localhost, so setup here is password-only. You can turn this off later in Server › Settings."
                        }
                    }
                }
            }
            if let Some(text) = error() {
                Alert { text }
            }
            div { class: "wizard-foot",
                span {}
                button { class: "btn-dialog btn-dialog-primary h-10", r#type: "submit", disabled: busy() || code().trim().is_empty(),
                    icons::ArrowRight {}
                    "Continue"
                }
            }
        }
    }
}

/// The first admin exists: their display name is given (P3-18), step 1 is
/// done, and the wizard goes on. Over plain HTTP without insecure cookies
/// the browser dropped the session, and the next page sends it to sign
/// in, which says why.
async fn admin_made(display_name: Option<String>) -> Route {
    if let Ok(client) = rpc::client().await {
        // Checked before the account was made; a failure now is the
        // connection's, and My profile can set it.
        if let Some(name) = display_name
            && let Err(e) =
                spoolframe_admin_api::MeApiClient::set_display_name(&*client, Some(name)).await
        {
            tracing::warn!(error = %e, "the first admin's display name was not set");
        }
        let _ =
            spoolframe_admin_api::AdminApiClient::setup_step_done(&*client, "admin".into()).await;
    }
    Route::Wizard {
        step: "server".into(),
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
                ". Nobody else can open this page once it exists."
            }
        }
    }
}

/// The first admin's display name and username, side by side where they
/// fit (WebSetup). The display name can still change once the account is
/// made: it is given when the step is finished.
#[component]
fn NameFields(display: Signal<String>, username: Signal<String>, disabled: bool) -> Element {
    rsx! {
        div { class: "grid grid-cols-[repeat(auto-fit,minmax(200px,1fr))] gap-3",
            Field { label: "Display name", value: display, autocomplete: "nickname", placeholder: "Optional" }
            Field { label: "Username", value: username, autocomplete: "username", disabled }
        }
    }
}

/// Setup with a password only, where no passkey can be made: `why`.
#[component]
fn PasswordSetup(name: String, why: String, setup_code: String) -> Element {
    let nav = navigator();
    let code = use_signal(|| setup_code.clone());
    let display = use_signal(String::new);
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let again = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let submit = move |e: FormEvent| {
        e.prevent_default();
        let shown = match crate::setup::display_name(&display()) {
            Ok(shown) => shown,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        if password() != again() {
            error.set(Some("The passwords are not the same.".into()));
            return;
        }
        busy.set(true);
        spawn(async move {
            match http::setup(&code(), &username(), &password()).await {
                Ok(_) => {
                    rpc::forget();
                    nav.replace(admin_made(shown).await);
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
            NameFields { display, username, disabled: false }
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

/// Setup as the board draws it: the username, then the ways to sign in,
/// the passkey first.
#[component]
fn MethodsSetup(name: String, setup_code: String) -> Element {
    let nav = navigator();
    let code = use_signal(|| setup_code.clone());
    let display = use_signal(String::new);
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
    // The passkey request for the username as typed, fetched once typing
    // pauses, so "Create a passkey" opens the prompt from the click itself:
    // Safari refuses a passkey prompt opened after a network request.
    let mut ahead = use_resource(move || async move {
        let typed = username();
        if typed.trim().is_empty() || !webauthn::supported() {
            return None;
        }
        admin::sleep(SETUP_REQUEST_PAUSE_MS).await;
        http::setup_passkey_begin(&code(), &typed)
            .await
            .ok()
            .map(|challenge| (typed, challenge, now()))
    });
    let create_passkey = move |_| {
        error.set(None);
        busy.set(true);
        // Opened here, before anything is awaited, when the request for
        // this username is in hand; fetched first (as browsers other than
        // Safari allow) only when it is not yet.
        let opened = ahead
            .read()
            .clone()
            .flatten()
            .filter(|(typed, _, at)| *typed == username() && now() - at < REQUEST_FRESH_SECS)
            .map(|(_, challenge, _)| {
                webauthn::open_create(&challenge.options).map(|prompt| (challenge, prompt))
            });
        spawn(async move {
            let created = async {
                let (challenge, prompt) = match opened {
                    Some(opened) => opened?,
                    None => {
                        let challenge = http::setup_passkey_begin(&code(), &username())
                            .await
                            .map_err(|e| e.explain())?;
                        let prompt = webauthn::open_create(&challenge.options)?;
                        (challenge, prompt)
                    }
                };
                let credential = prompt.answer().await?;
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
                Err(text) => {
                    error.set(Some(text));
                    // A request is tried once: the next click needs another.
                    ahead.restart();
                }
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
        let shown = match crate::setup::display_name(&display()) {
            Ok(shown) => shown,
            Err(why) => {
                error.set(Some(why));
                return;
            }
        };
        if made() {
            spawn(async move {
                nav.replace(admin_made(shown).await);
            });
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
                    nav.replace(admin_made(shown).await);
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
            NameFields { display, username, disabled: made() }
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
                    // A form, so the browser treats the two as a new
                    // password; Save is pressed, not submitted.
                    form { class: "method-form", onsubmit: move |e: FormEvent| e.prevent_default(),
                        Field { label: "Password", value: password, kind: "password", autocomplete: "new-password" }
                        Field { label: "Confirm password", value: again, kind: "password", autocomplete: "new-password" }
                        div { class: "flex gap-2",
                            button { class: "btn btn-teal h-[34px]", r#type: "button", disabled: busy(), onclick: save_password, "Save password" }
                            button { class: "btn h-[34px]", r#type: "button", onclick: move |_| typing.set(false), "Cancel" }
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
                            "If this device is lost or its passkeys are reset, nobody can sign in as admin, and getting back in needs shell access to the server. Add a password as well. You can also do it later in My profile, where a sign-in provider the server has can be linked too."
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

/// "1 library", "4 libraries".
fn count_of(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// A wizard step after the first admin (P3-10), for a signed-in admin.
#[component]
fn Wizard(step: String) -> Element {
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
        Some(Ok(Some(user))) if user.role != Role::Admin => {
            nav.replace(Route::Profile {});
            rsx! {}
        }
        Some(Ok(Some(_))) => rsx! {
            WizardStep { step }
        },
    }
}

#[component]
fn WizardStep(step: String) -> Element {
    let nav = navigator();
    // Read again at each step: the dots follow what is confirmed.
    let state = use_resource(use_reactive((&step,), |(step,)| async move {
        let _ = step;
        wizard::load_setup().await
    }));
    let mut error = use_signal(|| None::<String>);
    let mut done: Vec<String> = state
        .read()
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .map(|s| s.done.clone())
        .unwrap_or_default();
    // The admin is signed in: step 1 is behind them.
    if !done.iter().any(|d| d == "admin") {
        done.push("admin".into());
    }
    if step == "admin" {
        // Made already: their account is in My profile.
        nav.replace(Route::Profile {});
        return rsx! {};
    }
    if step == "resume" {
        let at = setup::resume(&done);
        return rsx! {
            WizardLayout { wizard::Resume { at, done } }
        };
    }
    if setup::step(&step).is_none() {
        return rsx! { NotFound { rest: vec![step] } };
    }
    let id = step.clone();
    let on_next = move |_| {
        let id = id.clone();
        spawn(async move {
            let confirmed = async {
                let client = rpc::client().await?;
                spoolframe_admin_api::AdminApiClient::setup_step_done(&*client, id.clone())
                    .await
                    .map_err(rpc::error)
            };
            match confirmed.await {
                Ok(_) => {
                    if let Some(next) = setup::next(&id) {
                        nav.push(Route::Wizard {
                            step: next.id.to_owned(),
                        });
                    }
                }
                Err(e) => error.set(Some(e.explain())),
            }
        });
    };
    rsx! {
        WizardLayout {
            Steps { current: step.clone(), done }
            section { class: "wizard-card",
                match step.as_str() {
                    "server" => rsx! { wizard::ServerForm { mode: Mode::Wizard, on_done: on_next } },
                    "plex" => rsx! { wizard::PlexForm { mode: Mode::Wizard, on_done: on_next } },
                    "paths" => rsx! { wizard::PathsForm { mode: Mode::Wizard, on_done: on_next } },
                    "playback" => rsx! { wizard::PlaybackForm { mode: Mode::Wizard, on_done: on_next } },
                    "access" => rsx! { wizard::AccessForm { mode: Mode::Wizard, on_done: on_next } },
                    _ => rsx! { wizard::DoneCard {} },
                }
                if let Some(text) = error() {
                    Alert { text }
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

/// What the signed-in user is shown as, in the sidebar and the bar: My
/// profile changes it (P3-03) without signing in again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShownName(pub Signal<String>);

/// What the phone bar calls a page that names its own subject, such as a
/// user's page (WebAdminPhone 5: "Alice"); `None` for the route's title.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarTitle(pub Signal<Option<String>>);

/// The frame itself. An admin's is the design's sidebar, or its bar on a
/// phone, with the pages behind the bar's menu button (the WebAdminPhone
/// board): the mark, the pages, and who is signed in. A consumer's is a
/// slim bar over My profile, their one page (WebProfile).
#[component]
fn SignedIn(user: UserInfo) -> Element {
    let nav = navigator();
    use_context_provider(|| user.clone());
    let route = use_route::<Route>();
    let shown = use_signal(|| {
        user.display_name
            .clone()
            .unwrap_or_else(|| user.username.clone())
    });
    use_context_provider(|| ShownName(shown));
    let bar_title = use_signal(|| None::<String>);
    use_context_provider(|| BarTitle(bar_title));
    let name = shown();
    let admin = user.role == Role::Admin;
    let avatar = avatar_class(&user);
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
        Route::AdminHome {} => "Overview",
        Route::Profile {} => "My profile",
        Route::Server {} => "Server",
        Route::Invites {} => "Invites",
        Route::ActivityPage {} => "Activity",
        Route::LogsPage {} => "Logs",
        Route::PlaybackPage {} | Route::PlaybackDetail { .. } => "Playback",
        Route::Libraries {} | Route::AddLibrary { .. } => "Libraries",
        Route::CatalogLibrary { .. } | Route::CatalogItem { .. } => "Libraries",
        Route::Settings { ref tab } if tab == "plex" => "Plex",
        Route::Settings { .. } | Route::SettingsHome {} => "Settings",
        _ => "Users",
    };
    let title = match (&route, bar_title()) {
        (Route::UserSessions { .. }, Some(named)) => named,
        _ => title.to_owned(),
    };
    let site = use_resource(site);
    let insecure = admin && site.read().as_ref().is_some_and(|s| s.insecure_cookies);
    if !admin {
        // A consumer has My profile alone (the WebProfile board): a slim
        // bar over one column, no sidebar.
        let server = site
            .read()
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_default();
        return rsx! {
            div { class: "min-h-screen bg-ctp-base text-ctp-text",
                header { class: "top-bar",
                    Link { class: "home-link flex min-w-0 items-center gap-2.5", to: Route::from(Landing::Profile),
                        icons::Mark { size: 28 }
                        Wordmark { class: "text-lg" }
                        if !server.is_empty() {
                            span { class: "top-bar-server hidden sm:inline", "{server}" }
                        }
                    }
                    span { class: "flex items-center gap-2.5",
                        span { class: "{avatar} avatar-md", "{initial(&name)}" }
                        b { class: "hidden max-w-[200px] truncate text-sm sm:inline", "{name}" }
                        button { class: "btn top-bar-out", aria_label: "Sign out", title: "Sign out", onclick: sign_out,
                            icons::LogOut {}
                            span { class: "hidden sm:inline", "Sign out" }
                        }
                    }
                }
                main { class: "consumer-page", Outlet::<Route> {} }
            }
        };
    }
    rsx! {
        // `content-start`: on a phone the bar and the page are two wrapped
        // lines, and stretched lines would leave a gap under the bar.
        div { class: "flex min-h-screen flex-wrap content-start bg-ctp-base text-ctp-text",
            aside { class: "sidebar hidden md:flex",
                // The mark goes to the admin's start, the Overview.
                Link { class: "home-link flex items-center gap-2.5 px-2", to: Route::from(Landing::Admin),
                    icons::Mark { size: 28 }
                    div { class: "flex flex-col",
                        Wordmark {}
                        span { class: "text-xs text-ctp-subtext0", "Admin" }
                    }
                }
                nav { class: "flex flex-col gap-0.5",
                    {link(Route::AdminHome {}, "Overview", false)}
                    {link(Route::Users {}, "Users and devices", false)}
                    {users_links(&route)}
                    {link(Route::Server {}, "Server", false)}
                    {server_links(&route)}
                    {link(plex_page(), "Plex", false)}
                    {link(Route::Profile {}, "My profile", true)}
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
                    button {
                        class: "icon-button -ml-1",
                        aria_label: "Menu",
                        aria_expanded: "{menu()}",
                        onclick: move |_| menu.toggle(),
                        icons::Menu {}
                    }
                    Link { class: "home-link flex", to: Route::from(Landing::Admin), aria_label: "Overview",
                        icons::Mark { size: 24 }
                    }
                    b { class: "font-display text-[17px]", "{title}" }
                }
                span { class: "flex items-center gap-3",
                    button { class: "text-ctp-subtext0", aria_label: "Sign out", onclick: sign_out, icons::LogOut {} }
                    span { class: "{avatar} avatar-md", "{initial(&name)}" }
                }
            }
            if menu() {
                div { class: "drawer-scrim md:hidden", aria_hidden: "true", onclick: move |_| menu.set(false) }
                // A link's click comes up here once it has navigated.
                nav { class: "drawer md:hidden", aria_label: "Admin", onclick: move |_| menu.set(false),
                    {link(Route::AdminHome {}, "Overview", false)}
                    {link(Route::Users {}, "Users and devices", false)}
                    {users_links(&route)}
                    {link(Route::Server {}, "Server", false)}
                    {server_links(&route)}
                    {link(plex_page(), "Plex", false)}
                    {link(Route::Profile {}, "My profile", true)}
                }
            }
            main { class: "box-border min-w-0 flex-[1_1_600px] max-w-[1200px] p-3.5 md:px-10 md:py-8",
                if insecure {
                    div { class: "banner", role: "alert",
                        icons::Warning {}
                        span {
                            b { "Insecure cookies are on. " }
                            "Sessions can be read by anyone watching the network."
                        }
                        Link { to: Route::Settings { tab: "security".into() }, "Turn off" }
                    }
                }
                Outlet::<Route> {}
            }
        }
    }
}

/// The sidebar's Plex (WebUsers, WebAdminPhone 2): Settings' Plex tab,
/// where the Plex Media Server and the admin's Plex account are.
fn plex_page() -> Route {
    Route::Settings { tab: "plex".into() }
}

/// Users and devices' sub-page in the sidebar, one step in: Invites
/// (P6-01).
fn users_links(route: &Route) -> Element {
    let here = matches!(route, Route::Invites {});
    rsx! {
        Link {
            class: if here { "nav-link nav-link-here nav-sub" } else { "nav-link nav-sub" },
            to: Route::Invites {},
            "Invites"
        }
    }
}

/// Server's sub-pages in the sidebar, one step in: Activity (P3-14),
/// Playback (P3-15), Logs (P3-14), Libraries (P3-11) and Settings.
fn server_links(route: &Route) -> Element {
    let pages = [
        (
            Route::ActivityPage {},
            "Activity",
            matches!(route, Route::ActivityPage {}),
        ),
        (
            Route::PlaybackPage {},
            "Playback",
            matches!(route, Route::PlaybackPage {} | Route::PlaybackDetail { .. }),
        ),
        (
            Route::LogsPage {},
            "Logs",
            matches!(route, Route::LogsPage {}),
        ),
        (
            Route::Libraries {},
            "Libraries",
            matches!(
                route,
                Route::Libraries {}
                    | Route::AddLibrary { .. }
                    | Route::CatalogLibrary { .. }
                    | Route::CatalogItem { .. }
            ),
        ),
        (
            Route::Settings {
                tab: "server".into(),
            },
            "Settings",
            // Settings' Plex tab is the sidebar's Plex, not Settings.
            matches!(route, Route::Settings { tab } if tab != "plex")
                || matches!(route, Route::SettingsHome {}),
        ),
    ];
    rsx! {
        for (to, label, here) in pages {
            Link {
                key: "{label}",
                class: if here { "nav-link nav-link-here nav-sub" } else { "nav-link nav-sub" },
                to,
                "{label}"
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
